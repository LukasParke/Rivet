<script>
	/** @typedef {{ ok: boolean; details?: Record<string,string>; error?: string }} IndexerTestResult */
	/** @typedef {{ ok: boolean; torznab: IndexerTestResult[]; newznab: IndexerTestResult[] }} ValidateResponse */

	let torznabUrl = '';
	let torznabKey = '';
	let torznabName = 'Torznab';

	let newznabUrl = '';
	let newznabKey = '';
	let newznabName = 'Newznab';

	let includeTorznab = true;
	let includeNewznab = true;

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
		const p = { torznab: [], newznab: [] };
		if (includeTorznab) {
			p.torznab.push({ name: torznabName, url: torznabUrl, apiKey: torznabKey, username: '', password: '' });
		}
		if (includeNewznab) {
			p.newznab.push({ name: newznabName, url: newznabUrl, apiKey: newznabKey, username: '', password: '' });
		}
		return p;
	}

	async function validate() {
		busy = true;
		error = null;
		result = null;
		try {
			const res = await fetch('/api/v1/setup/indexers/validate', {
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
			const res = await fetch('/api/v1/setup/indexers/save', {
				method: 'POST',
				headers: { 'content-type': 'application/json', ...csrfHeader() },
				body: JSON.stringify(payload())
			});
			const data = await res.json().catch(() => null);
			if (!res.ok) {
				if (data?.torznab || data?.newznab) {
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
	<h2>Indexers</h2>
	<p class="muted">We’ll run a sample search and parse the RSS response.</p>

	<h3>Torznab</h3>
	<label><input type="checkbox" bind:checked={includeTorznab} /> Enable</label>
	<div class="row" style="margin-top: 8px;">
		<input placeholder="Name" bind:value={torznabName} disabled={!includeTorznab} />
		<input placeholder="Base URL (e.g. https://indexer)" bind:value={torznabUrl} disabled={!includeTorznab} />
		<input placeholder="API key" type="password" bind:value={torznabKey} disabled={!includeTorznab} />
	</div>

	<h3 style="margin-top: 16px;">Newznab</h3>
	<label><input type="checkbox" bind:checked={includeNewznab} /> Enable</label>
	<div class="row" style="margin-top: 8px;">
		<input placeholder="Name" bind:value={newznabName} disabled={!includeNewznab} />
		<input placeholder="Base URL (e.g. https://indexer)" bind:value={newznabUrl} disabled={!includeNewznab} />
		<input placeholder="API key" type="password" bind:value={newznabKey} disabled={!includeNewznab} />
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
			<pre>{JSON.stringify(result, null, 2)}</pre>
		</div>
	{/if}
</div>
