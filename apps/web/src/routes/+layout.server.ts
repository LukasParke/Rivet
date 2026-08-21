// Data load only — setup-mode/auth gating lives in src/hooks.server.ts.
export async function load({ fetch }) {
	let dryRun = false;
	let dryRunFixturePathSet = false;
	try {
		const res = await fetch('/api/v1/setup/status');
		if (res.ok) {
			const data = await res.json();
			dryRun = !!data.dryRun;
			dryRunFixturePathSet = !!data.dryRunFixturePathSet;
		}
	} catch {
		// ignore — banner simply stays hidden
	}
	return { dryRun, dryRunFixturePathSet };
}
