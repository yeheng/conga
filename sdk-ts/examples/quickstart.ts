/**
 * The 20-line acceptance demo: one turn, every event, end to end.
 *
 *   npx tsx examples/quickstart.ts "总结当前目录的 git 状态"
 *
 * Requires the `conga` binary on PATH (or CONGA_BIN set) and a configured
 * ~/.conga environment (see the repo README, "Quick start").
 */
import { exec, isDoneWithSummary } from "../src/index.js";

const task = process.argv[2] ?? "用一句话介绍你自己";
const run = exec({ task, congaBin: process.env.CONGA_BIN });

for await (const ev of run) {
  if (ev.type === "content") process.stdout.write(ev.content);
  else if (ev.type === "tool_start") console.error(`\n[tool] ${ev.name} ${ev.arguments}`);
  else if (ev.type === "tool_end") console.error(`[tool] ${ev.name} done`);
  else if (ev.type === "error") console.error(`\n[error] ${ev.message}`);
  else if (isDoneWithSummary(ev)) {
    console.error(`\n[done] in=${ev.usage_in} out=${ev.usage_out} ${ev.elapsed_ms}ms`);
  }
}

const { exitCode } = await run.wait();
console.error(`\n[exit] ${exitCode} (0 done · 1 turn error · 130 aborted · 2 usage)`);
