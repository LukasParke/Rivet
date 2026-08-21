// Data load only — routing guards live in src/hooks.server.ts.
/** @typedef {{ id?: string, status?: string, clientKind?: string, clientName?: string, verifyStatus?: string | null, lastError?: string | null, [k: string]: unknown }} ReviewItem */

/** @returns {Promise<{ items: ReviewItem[] }>} */
export async function load({ fetch }) {
	try {
		const res = await fetch('/api/v1/acquisitions?status=needs_review&limit=100');
		if (!res.ok) return { items: [] };
		const data = await res.json().catch(() => null);
		return { items: Array.isArray(data) ? data : [] };
	} catch {
		return { items: [] };
	}
}
