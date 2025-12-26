<script>
	/** @typedef {{ ok: boolean; details?: Record<string,string>; error?: string }} ProviderTestResult */
	/** @typedef {{ ok: boolean; tmdb?: ProviderTestResult; tvdb?: ProviderTestResult }} ValidateResponse */

	let tmdbBaseUrl = 'https://api.themoviedb.org';
	let tmdbKey = '';
	let includeTMDB = true;

	let tvdbBaseUrl = 'https://api4.thetvdb.com';
	let tvdbKey = '';
	let includeTVDB = false;

	/** @type {ValidateResponse | null} */
	let result = null;
	let error = null;
	let busy = false;

	function csrfHeader() {
		const t = sessionStorage.getItem('csrf');
		return t ? { 'x-csrf-token': t } : {};
	}

	function payload() {
		/** @type {any} */
		const p = {};
		if (includeTMDB) p.tmdb = { baseUrl: tmdbBaseUrl, apiKey: tmdbKey };
		if (includeTVDB) p.tvdb = { baseUrl: tvdbBaseUrl, apiKey: tvdbKey };
		return p;
	}

	async function validate() {
		busy = true;
		error = null;
		result = null;
		try {
			const res = await fetch('/api/v1/setup/metadata/validate', {
				method: 'POST',
				headers: { 'content-type': 'application/json', ...csrfHeader() },
				body: JSON.stringify(payload())
			});
			const data = await res.json().catch(() => null);
			if (!res.ok) throw new Error(data?.message || data?.error || 'validate_failed');
			result = /** @type {ValidateResponse} */ (data);
		} catch (e) {
			error = e instanceof Error ? e.message : String(e);
		} finally {
			busy = false;
		}
	}

	async function save() {
		busy = true;
		error = null;
		try {
			const res = await fetch('/api/v1/setup/metadata/save', {
				method: 'POST',
				headers: { 'content-type': 'application/json', ...csrfHeader() },
				body: JSON.stringify(payload())
			});
			const data = await res.json().catch(() => null);
			if (!res.ok) {
				if (data?.tmdb || data?.tvdb) {
					result = /** @type {ValidateResponse} */ (data);
					return;
				}
				throw new Error(data?.message || data?.error || 'save_failed');
			}
			window.location.href = '/setup';
		} catch (e) {
			error = e instanceof Error ? e.message : String(e);
		} finally {
			busy = false;
		}
	}
</script>

<div class="card">
	<h2>Metadata providers</h2>
	<p class="muted">We’ll validate your API keys and set request budgets in a later step.</p>

	<h3>TMDB</h3>
	<label><input type="checkbox" bind:checked={includeTMDB} /> Enable</label>
	<div class="row" style="margin-top: 8px;">
		<input placeholder="Base URL" bind:value={tmdbBaseUrl} disabled={!includeTMDB} />
		<input placeholder="API key (optional - leave blank for shared key)" type="password" bind:value={tmdbKey} disabled={!includeTMDB} />
	</div>
	{#if includeTMDB}
		<p class="muted" style="margin-top: 4px; font-size: 0.9em;">
			Optional: Leave blank to use shared key (rate-limited). 
			<a href="https://www.themoviedb.org/settings/api" target="_blank" rel="noopener">Get your own free API key</a> (takes 2 minutes).
		</p>
	{/if}

	<h3 style="margin-top: 16px;">TVDB</h3>
	<label><input type="checkbox" bind:checked={includeTVDB} /> Enable</label>
	<div class="row" style="margin-top: 8px;">
		<input placeholder="Base URL" bind:value={tvdbBaseUrl} disabled={!includeTVDB} />
		<input placeholder="API key" type="password" bind:value={tvdbKey} disabled={!includeTVDB} />
	</div>

	<div class="row" style="margin-top: 12px;">
		<button disabled={busy} on:click={validate}>Validate</button>
		<button class="secondary" disabled={busy || !result?.ok} on:click={save}>Save</button>
	</div>

	{#if error}
		<p class="error">{error}</p>
	{/if}

	{#if result}
		<div style="margin-top: 12px;">
			<p><strong>Overall:</strong> {result.ok ? 'OK' : 'Needs changes'}</p>
			{#if result.tmdb?.details?.warning}
				<p class="muted" style="margin-top: 8px;">⚠️ {result.tmdb.details.warning}</p>
			{/if}
			<pre>{JSON.stringify(result, null, 2)}</pre>
		</div>
	{/if}
</div>
