// Pure helpers for the Linear credential panel. Ported from $REF/desktop/frontend/src/creds.ts.
// The daemon does the real validation; these are only light client-side affordances.
import type { CredentialStatus } from "./bindings";

// tokenLooksValid is a basic sanity check before enabling Save — a Linear personal API key is
// a long "lin_api_…" string. It is intentionally lenient; the daemon is the source of truth.
export function tokenLooksValid(token: string): boolean {
  const t = token.trim();
  if (t.length === 0) return false;
  return t.startsWith("lin_") || t.length >= 40;
}

// credentialSummary is the status line for the panel header.
export function credentialSummary(s: CredentialStatus | null): string {
  if (!s) return "Checking…";
  if (!s.has_token) return "No Linear token set";
  return `Linear token stored (${s.backend})`;
}
