// Initial data for the /series hub page — data loads only; routing guards
// live in src/hooks.server.ts.
export async function load({ fetch }) {
	try {
		const res = await fetch('/api/v1/wanted/missing?pageSize=50');
		if (!res.ok) return { missing: [], total: 0 };
		const body = await res.json();
		return { missing: Array.isArray(body?.items) ? body.items : [], total: body?.total ?? 0 };
	} catch {
		return { missing: [], total: 0 };
	}
}
