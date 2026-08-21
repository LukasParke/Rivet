// Initial week of episodes plus the monitored-missing backlog for missing
// badges. Data loads only — routing guards live in src/hooks.server.ts.
export async function load({ fetch }) {
	const today = new Date();
	// Monday-based current week (UTC).
	const monday = new Date(Date.UTC(today.getUTCFullYear(), today.getUTCMonth(), today.getUTCDate()));
	monday.setUTCDate(monday.getUTCDate() - ((monday.getUTCDay() + 6) % 7));
	const sunday = new Date(monday);
	sunday.setUTCDate(sunday.getUTCDate() + 6);
	const start = monday.toISOString().slice(0, 10);
	const end = sunday.toISOString().slice(0, 10);

	try {
		const [calRes, missRes] = await Promise.all([
			fetch(`/api/v1/calendar?start=${start}&end=${end}`),
			fetch('/api/v1/calendar/missing?limit=500')
		]);
		const episodes = calRes.ok ? await calRes.json().catch(() => []) : [];
		const missingRaw = missRes.ok ? await missRes.json().catch(() => []) : [];
		const missingIds = Array.isArray(missingRaw) ? missingRaw.map((e) => e.id) : [];
		return {
			start,
			end,
			episodes: Array.isArray(episodes) ? episodes : [],
			missing: [...new Set(missingIds)]
		};
	} catch {
		return { start, end, episodes: [], missing: [] };
	}
}
