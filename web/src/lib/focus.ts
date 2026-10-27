import { useEffect, useRef } from "react";

/** A control another view asked to bring into view (e.g. a warning's "move to another server"). */
interface FocusRequest {
  instanceId: string;
  target: "server";
}

const EVENT = "mc:focus-request";
let pending: FocusRequest | null = null;

/** Ask the `target` control of `instanceId` to show itself; it may mount later (the sheet opens first). */
export function requestFocus(instanceId: string, target: FocusRequest["target"]) {
  pending = { instanceId, target };
  window.dispatchEvent(new Event(EVENT));
}

/** Runs `onFocus` once per request for this control, whether it was mounted already or mounts after. */
export function useFocusRequest(instanceId: string, target: FocusRequest["target"], onFocus: () => void) {
  const cb = useRef(onFocus);
  cb.current = onFocus;
  useEffect(() => {
    const take = () => {
      if (pending?.instanceId === instanceId && pending.target === target) {
        pending = null;
        cb.current();
      }
    };
    take();
    window.addEventListener(EVENT, take);
    return () => window.removeEventListener(EVENT, take);
  }, [instanceId, target]);
}
