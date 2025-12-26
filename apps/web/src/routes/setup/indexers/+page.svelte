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
	<p class="muted">We'll run a sample search and parse the RSS response.</p>

	<div style="background: #f0f0f0; padding: 12px; border-radius: 4px; margin-bottom: 16px;">
		<h3 style="margin-top: 0;">Quick Setup: Prowlarr (Recommended)</h3>
		<p style="margin: 8px 0; font-size: 0.9em;">
			Prowlarr manages multiple indexers and provides a single Torznab endpoint. This is the easiest way to get started.
		</p>
		<p style="margin: 8px 0; font-size: 0.9em;">
			<strong>Install:</strong> <code>docker run -d --name prowlarr -p 9696:9696 lscr.io/linuxserver/prowlarr</code>
		</p>
		<p style="margin: 8px 0; font-size: 0.9em;">
			Then configure indexers in Prowlarr's web UI (http://localhost:9696), and add Prowlarr below as a Torznab indexer.
		</p>
		<p style="margin: 8px 0; font-size: 0.85em; color: #666;">
			<strong>Important:</strong> Use the base Prowlarr URL (e.g., <code>http://localhost:9696</code>). The Torznab endpoint is automatically at <code>/api</code> relative to the base URL. Do not include <code>/api</code> or <code>/prowlarr</code> in the URL.
		</p>
		<button 
			class="secondary" 
			style="margin-top: 8px;"
			on:click={() => {
				torznabUrl = 'http://localhost:9696';
				torznabName = 'Prowlarr';
				torznabKey = '';
				includeTorznab = true;
			}}
		>
			Pre-fill Prowlarr settings
		</button>
		<p style="margin: 8px 0; font-size: 0.85em; color: #666;">
			💡 Get your Prowlarr API key from Settings → General → API Key
		</p>
	</div>

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
