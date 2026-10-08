/** Lead keys identify manager work, never tracker tickets (STUDIO-1145). */
export function isLeadRun(key: string): boolean {
  return key.startsWith("lead:");
}
