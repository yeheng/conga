//! RAG answer generation on conga's StreamFn (no agent loop).

use std::sync::Arc;

use conga::{AgentMessage, ModelSpec, StreamChunk, StreamFn};

use crate::store::Hit;

pub const ASK_SYSTEM: &str = "你是个人知识库问答助手。仅依据用户提供的编号资料回答问题;引用时标注 [编号];资料中没有的信息,直接说明知识库中没有,不要编造。";

pub fn build_context(hits: &[Hit]) -> String {
    let mut s = String::from("资料:\n");
    for (i, h) in hits.iter().enumerate() {
        s.push_str(&format!(
            "[{}] ({}, 第{}块)\n{}\n\n",
            i + 1,
            h.path,
            h.ordinal + 1,
            h.content
        ));
    }
    s
}

pub async fn run_ask(
    stream: Arc<dyn StreamFn>,
    model: ModelSpec,
    question: &str,
    hits: &[Hit],
    on_delta: &mut dyn FnMut(&str),
) -> anyhow::Result<()> {
    use futures_util::StreamExt;
    let message = AgentMessage::user(format!("{}\n问题: {}", build_context(hits), question));
    let mut s = stream.stream(&model, &[message], ASK_SYSTEM, &[], None);
    while let Some(chunk) = s.next().await {
        match chunk {
            StreamChunk::TextDelta(t) => on_delta(&t),
            StreamChunk::Done => break,
            StreamChunk::Error(e) => anyhow::bail!("生成失败: {e}"),
            _ => {}
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::pin::Pin;

    use conga::{ProviderApi, ToolDefinition};
    use futures_util::Stream;

    fn hit(path: &str, content: &str) -> Hit {
        Hit {
            source: "notes".into(),
            path: path.into(),
            ordinal: 0,
            content: content.into(),
            score: 0.9,
        }
    }

    #[test]
    fn context_numbers_hits() {
        let ctx = build_context(&[hit("/a.md", "alpha"), hit("/b.md", "beta")]);
        assert!(ctx.contains("[1]"), "{ctx}");
        assert!(ctx.contains("/a.md"));
        assert!(ctx.contains("beta"));
    }

    /// Fake StreamFn yielding a fixed answer.
    struct Fake;
    impl StreamFn for Fake {
        fn stream(
            &self,
            _model: &ModelSpec,
            _messages: &[AgentMessage],
            _system: &str,
            _tools: &[ToolDefinition],
            _signal: Option<conga::CancelSignal>,
        ) -> Pin<Box<dyn Stream<Item = StreamChunk> + Send>> {
            Box::pin(futures_util::stream::iter(vec![
                StreamChunk::TextDelta("答案".into()),
                StreamChunk::TextDelta("[1]".into()),
                StreamChunk::Done,
            ]))
        }
    }

    #[tokio::test]
    async fn ask_streams_deltas_and_finishes() {
        let hits = vec![hit("/a.md", "alpha")];
        let mut out = String::new();
        run_ask(
            Arc::new(Fake),
            ModelSpec {
                id: "m".into(),
                api: ProviderApi::OpenAiCompat,
                max_tokens: 64,
            },
            "q?",
            &hits,
            &mut |d| out.push_str(d),
        )
        .await
        .unwrap();
        assert_eq!(out, "答案[1]");
    }

    #[test]
    fn context_of_an_empty_hit_list_is_only_the_header() {
        assert_eq!(build_context(&[]), "资料:\n");
    }

    #[test]
    fn context_uses_one_based_hit_and_block_numbers() {
        let mut h = hit("/a.md", "alpha");
        h.ordinal = 2;
        let ctx = build_context(&[h]);
        assert!(ctx.contains("[1]"), "{ctx}");
        assert!(ctx.contains("第3块"), "the ordinal is one-based: {ctx}");
    }

    /// Fake StreamFn that fails mid-stream.
    struct Failing;
    impl StreamFn for Failing {
        fn stream(
            &self,
            _model: &ModelSpec,
            _messages: &[AgentMessage],
            _system: &str,
            _tools: &[ToolDefinition],
            _signal: Option<conga::CancelSignal>,
        ) -> Pin<Box<dyn Stream<Item = StreamChunk> + Send>> {
            Box::pin(futures_util::stream::iter(vec![
                StreamChunk::TextDelta("部分答案".into()),
                StreamChunk::Error("provider exploded".into()),
            ]))
        }
    }

    #[tokio::test]
    async fn ask_fails_when_the_provider_stream_errors() {
        let hits = vec![hit("/a.md", "alpha")];
        let mut out = String::new();
        let err = run_ask(
            Arc::new(Failing),
            ModelSpec {
                id: "m".into(),
                api: ProviderApi::OpenAiCompat,
                max_tokens: 64,
            },
            "q?",
            &hits,
            &mut |d| out.push_str(d),
        )
        .await
        .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("生成失败"), "msg: {msg}");
        assert!(msg.contains("provider exploded"), "msg: {msg}");
        assert_eq!(out, "部分答案", "the text before the error was delivered");
    }
}
