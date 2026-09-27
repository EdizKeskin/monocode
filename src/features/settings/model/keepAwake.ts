import { invoke, isTauri } from "@tauri-apps/api/core";
import { useEffect, useRef, useSyncExternalStore } from "react";
import { IS_WIN } from "../../../platform/tauri/platform";
import { sessionNeedsInput, type Session } from "../../sessions/model/session";
import {
  loadKeepAwakeEnabled,
  subscribeKeepAwakeEnabled,
} from "./settings";

// React StrictMode can replace a controller before its final IPC settles.
let nativeQueue: Promise<void> = Promise.resolve();

function sendNative(enabled: boolean): Promise<void> {
  const operation = nativeQueue.then(() =>
    invoke<void>("set_keep_awake", { enabled }),
  );
  nativeQueue = operation.then(() => undefined, () => undefined);
  return operation;
}

/** A pending approval/question is not active work even when busy is still set. */
export function isWorkingSession(session: Session): boolean {
  return (
    !!session.busy && !session.worktreeRemoved && !sessionNeedsInput(session)
  );
}

/** Each webview reports only its own activity; the native command combines windows. */
export function createKeepAwakeController(
  send: (enabled: boolean) => Promise<unknown>,
) {
  let desired = false;
  let disposed = false;
  let failed = false;
  let pending = Promise.resolve();

  const set = (enabled: boolean) => {
    if (desired === enabled && !failed) return;
    desired = enabled;
    failed = false;
    // Preserve transition order if a turn finishes before the first IPC returns.
    pending = pending.then(async () => {
      try {
        await send(enabled);
      } catch {
        if (desired !== enabled) return;
        try {
          await send(enabled);
        } catch {
          if (desired === enabled) failed = true;
        }
      }
    });
  };

  return {
    update(enabled: boolean, sessions: readonly Session[]) {
      if (!disposed) set(enabled && sessions.some(isWorkingSession));
    },
    release() {
      if (disposed) return;
      set(false);
      disposed = true;
    },
    settled: () => pending,
  };
}

export function useKeepAwake(sessions: readonly Session[]): void {
  const enabled = useSyncExternalStore(
    subscribeKeepAwakeEnabled,
    loadKeepAwakeEnabled,
    () => false,
  );
  const controller =
    useRef<ReturnType<typeof createKeepAwakeController> | null>(null);

  useEffect(() => {
    if (!IS_WIN || !isTauri()) return;
    const current = createKeepAwakeController(sendNative);
    controller.current = current;
    const release = () => current.release();
    window.addEventListener("pagehide", release);
    window.addEventListener("beforeunload", release);
    return () => {
      window.removeEventListener("pagehide", release);
      window.removeEventListener("beforeunload", release);
      current.release();
      if (controller.current === current) controller.current = null;
    };
  }, []);

  useEffect(() => {
    controller.current?.update(enabled, sessions);
  }, [enabled, sessions]);
}
