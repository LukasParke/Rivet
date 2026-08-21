<script>
	import { apiFetch } from '$lib/api';

	/** @typedef {{ ok: boolean; details?: Record<string,string>; error?: string }} ClientTestResult */
	/** @typedef {{ ok: boolean; qbittorrent?: ClientTestResult; nzbget?: ClientTestResult; sabnzbd?: ClientTestResult }} ValidateResponse */

	let qbUrl = 'http://qbittorrent:8080';
	let qbUser = '';
	let qbPass = '';

	let nzbUrl = 'http://nzbget:6789';
	let nzbUser = '';
	let nzbPass = '';

	let sabUrl = 'http://sabnzbd:8080';
	let sabKey = '';

	let includeQb = true;
	let includeNzb = true;
	let includeSab = false;

	/** @type {ValidateResponse | null} */
	let result = null;
	let error = null;
	let busy = false;


	function payload() {
		/** @type {any} */
		const p = {};
		if (includeQb) p.qbittorrent = { url: qbUrl, username: qbUser, password: qbPass };
		if (includeNzb) p.nzbget = { url: nzbUrl, username: nzbUser, password: nzbPass };
		if (includeSab) p.sabnzbd = { url: sabUrl, apiKey: sabKey };
		return p;
	}

	async function validate() {
		busy = true;
		error = null;
		result = null;
		try {
			const res = await apiFetch('/api/v1/setup/clients/validate', {
				method: 'POST',
				headers: { 'content-type': 'application/json' },
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
			const res = await apiFetch('/api/v1/setup/clients/save', {
				method: 'POST',
				headers: { 'content-type': 'application/json' },
				body: JSON.stringify(payload())
			});
			const data = await res.json().catch(() => null);
			if (!res.ok) {
				if (data?.qbittorrent || data?.nzbget || data?.sabnzbd) {
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
	<h2>Download clients</h2>
	<p class="muted">Configure at least one torrent and/or usenet client. Credentials are stored encrypted at rest.</p>

	<h3>qBittorrent</h3>
	<label><input type="checkbox" bind:checked={includeQb} /> Enable</label>
	<div class="row" style="margin-top: 8px;">
		<input placeholder="URL" bind:value={qbUrl} disabled={!includeQb} />
		<input placeholder="Username" bind:value={qbUser} disabled={!includeQb} />
		<input placeholder="Password" type="password" bind:value={qbPass} disabled={!includeQb} />
	</div>

	<h3 style="margin-top: 16px;">NZBGet</h3>
	<label><input type="checkbox" bind:checked={includeNzb} /> Enable</label>
	<div class="row" style="margin-top: 8px;">
		<input placeholder="URL" bind:value={nzbUrl} disabled={!includeNzb} />
		<input placeholder="Username" bind:value={nzbUser} disabled={!includeNzb} />
		<input placeholder="Password" type="password" bind:value={nzbPass} disabled={!includeNzb} />
	</div>

	<h3 style="margin-top: 16px;">SABnzbd</h3>
	<label><input type="checkbox" bind:checked={includeSab} /> Enable</label>
	<div class="row" style="margin-top: 8px;">
		<input placeholder="URL" bind:value={sabUrl} disabled={!includeSab} />
		<input placeholder="API key" type="password" bind:value={sabKey} disabled={!includeSab} />
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
