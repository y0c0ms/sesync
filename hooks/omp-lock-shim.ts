// omp extension: forwards turn events to the sesync-lock hook. No logic lives here; the rules are in sesync-lock/src/main.rs (hook).
import { spawnSync } from "node:child_process";
export default function (pi: any): void {
  const hs = process.env.SESYNC_LOCK ?? "sesync-lock";
  // The harness name in the lock: omp by default; Pi-based harnesses (spidair) set SESYNC_HARNESS.
  const harness = process.env.SESYNC_HARNESS ?? "omp";
  const id = (ctx: any): string => {
    const file: string = ctx?.sessionManager?.getSessionFile?.() ?? "";
    return file.match(/_([0-9a-f-]{36})\.jsonl$/i)?.[1] ?? "";
  };
  const call = (ev: string, ctx: any, body = "{}") => {
    const sid = id(ctx);
    if (!sid) return;
    spawnSync(hs, ["hook", ev, harness, "--pid", String(process.pid), "--session", sid], { input: body });
  };
  pi.on("turn_start", (_e: any, ctx: any) => call("turn_start", ctx));
  pi.on("turn_end", (_e: any, ctx: any) => call("turn_end", ctx));
  // An assistant message ending releases the lock even if turn_end is late.
  pi.on("message_end", (e: any, ctx: any) =>
    call("message_end", ctx, JSON.stringify({ message: { role: e?.message?.role } })));
  // A run that will auto-retry keeps the lock; the hook decides from willContinue.
  pi.on("agent_end", (e: any, ctx: any) =>
    call("agent_end", ctx, JSON.stringify({ willContinue: !!e?.willContinue })));
  pi.on("session_shutdown", (_e: any, ctx: any) => call("session_shutdown", ctx));
}
