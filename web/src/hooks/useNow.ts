import { useEffect, useState } from "react";

// useNow returns Date.now() and re-renders the caller every `intervalMs`,
// driving live runtime/countdown labels between API polls (design §10.2).
export function useNow(intervalMs = 1000): number {
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    const id = setInterval(() => setNow(Date.now()), intervalMs);
    return () => clearInterval(id);
  }, [intervalMs]);
  return now;
}
