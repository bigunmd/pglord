/**
 * Surface the problem+json `detail` the server sends, falling back to the
 * stable `code` and then to the transport error.
 *
 * `detail` is human text and may change; `code` is the machine contract. Both
 * beat "Request failed", which tells the user nothing they can act on.
 */
export function problemDetail(err: unknown): string {
  const e = err as { error?: { detail?: string; code?: string }; message?: string };
  return e?.error?.detail ?? e?.error?.code ?? e?.message ?? 'Request failed';
}
