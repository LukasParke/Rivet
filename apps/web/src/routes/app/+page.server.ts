// Data load only — routing guards live in src/hooks.server.ts.
export async function load({ fetch }) {
	try {
		const res = await fetch('/api/v1/setup/status');
		if (!res.ok) return { dryRun: false, setupComplete: false };
		const data = await res.json();
		return {
			dryRun: !!data.dryRun,
			setupComplete: !!data.setupComplete
		};
	} catch {
		return { dryRun: false, setupComplete: false };
	}
}
