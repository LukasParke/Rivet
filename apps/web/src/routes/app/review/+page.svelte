<script>
	import { onDestroy } from 'svelte';
	import { apiFetch } from '$lib/api';
	import { connectWS } from '$lib/ws';

	export let data;

	let items = data?.items ?? [];
	let busy = false;
	let error = null;
	let wsStatus = 'disconnected';

	/** @type {Record<string, string>} */
	let overridePaths = {};
	/** @type {Record<string, boolean>} */
	let deleteFiles = {};
	/** @type {Record<string, string>} */
	let blacklistReasons = {};
	/** @type {Record<string, boolean>} */
	let blacklistGlobal = {};

	let wsHandle = null;
	let refreshTimer = null;

	const REFRESH_DEBOUNCE_MS = 500;

	async function refresh() {
		busy = true;
		error = null;
		try {
			const res = await apiFetch('/api/v1/acquisitions?status=needs_review&limit=100');
			const resData = await res.json().catch(() => null);
			if (!res.ok) throw new Error(resData?.error || 'fetch_failed');
			items = Array.isArray(resData) ? resData : [];
		} catch (e) {
			error = e instanceof Error ? e.message : String(e);
		} finally {
			busy = false;
		}
	}

	function scheduleRefresh() {
		if (refreshTimer !== null) return;
		refreshTimer = setTimeout(() => {
			refreshTimer = null;
			refresh();
		}, REFRESH_DEBOUNCE_MS);
	}

	/** Only lifecycle-changing events justify a refetch; progress spam does not. */
	function eventWarrantsRefresh(type) {
		if (typeof type !== 'string') return false;
		if (type === 'download.progress') return false;
		return (
			type.startsWith('download.') ||
			type.startsWith('import.') ||
			type.startsWith('verify.')
		);
	}

	function openSocket() {
		wsHandle?.close();
		wsHandle = connectWS({
			channels: ['downloads', 'imports', 'jobs', 'audit'],
			onStatus: (status) => (wsStatus = status),
			onEvent: (msg) => {
				if (eventWarrantsRefresh(msg.type)) scheduleRefresh();
			}
		});
	}

	async function act(id, action, body = null, query = '') {
		busy = true;
		error = null;
		try {
			const res = await apiFetch(`/api/v1/acquisitions/${id}/actions/${action}${query}`, {
				method: 'POST',
				headers: { 'content-type': 'application/json' },
				body: body ? JSON.stringify(body) : undefined
			});
			const resData = await res.json().catch(() => null);
			if (!res.ok) throw new Error(resData?.error || 'action_failed');
			await refresh();
		} catch (e) {
			error = e instanceof Error ? e.message : String(e);
		} finally {
			busy = false;
		}
	}

	openSocket();

	onDestroy(() => {
		wsHandle?.close();
		wsHandle = null;
		if (refreshTimer !== null) {
			clearTimeout(refreshTimer);
			refreshTimer = null;
		}
	});
</script>

<div class="container">
	<div class="row" style="justify-content: space-between; align-items: center;">
		<div>
			<h1>Review Queue</h1>
			<p class="muted">Acquisitions in <code>needs_review</code>. Status: {wsStatus}</p>
		</div>
		<div class="row">
			<a href="/app"><button class="secondary">Back to app</button></a>
			<button class="secondary" disabled={busy} on:click={refresh}>Refresh</button>
			<button class="secondary" disabled={busy} on:click={openSocket}>Reconnect WS</button>
		</div>
	</div>

	{#if error}
		<p class="error">{error}</p>
	{/if}

	{#if !items?.length}
		<div class="card" style="margin-top: 12px;">
			<p class="muted">No items in review.</p>
		</div>
	{:else}
		<div style="display: grid; gap: 12px; margin-top: 12px;">
			{#each items as a (a.id)}
				<div class="card">
					<div class="row" style="justify-content: space-between; align-items: flex-start;">
						<div>
							<div><strong>{a.id}</strong></div>
							<div class="muted">
								client: <code>{a.clientKind}</code> / <code>{a.clientName}</code>
								| kind: <code>{a.kind}</code>
								| verify: <code>{a.verifyStatus ?? ''}</code>
							</div>
							{#if a.lastError}
								<div class="error">lastError: {a.lastError}</div>
							{/if}
						</div>
						<div class="row" style="gap: 8px;">
							<button class="secondary" disabled={busy} on:click={() => act(a.id, 'reverify')}>Re-verify</button>
							<button class="secondary" disabled={busy} on:click={() => act(a.id, 'reimport')}>Re-import</button>
							<button class="secondary" disabled={busy} on:click={() => act(a.id, 'accept_verified')}>Accept + import</button>
						</div>
					</div>

					<details style="margin-top: 10px;">
						<summary class="muted">Evidence</summary>
						<div style="margin-top: 10px;">
							<div class="muted">sourceUrl: <code>{a.sourceURL ?? ''}</code></div>
							<div class="muted">externalId: <code>{a.externalID ?? ''}</code></div>
							<div class="muted">contentPath: <code>{a.contentPath ?? ''}</code></div>
							<div class="muted">verifiedPath: <code>{a.verifiedPath ?? ''}</code></div>
							<div class="muted">quarantinedPath: <code>{a.quarantinedPath ?? ''}</code></div>
							<div class="muted">progress: <code>{a.progress ?? ''}</code></div>
							<pre style="max-height: 240px; overflow: auto; margin-top: 10px;">{JSON.stringify(a, null, 2)}</pre>
						</div>
					</details>

					<div class="row" style="margin-top: 10px;">
						<input
							placeholder="override importedPath (admin)"
							value={overridePaths[a.id] ?? ''}
							on:input={(e) => (overridePaths[a.id] = e.currentTarget.value)}
						/>
						<button
							class="secondary"
							disabled={busy || !(overridePaths[a.id] ?? '').trim()}
							on:click={() => act(a.id, 'override_imported', { importedPath: (overridePaths[a.id] ?? '').trim() })}
						>
							Mark imported
						</button>
					</div>

					<div class="row" style="margin-top: 10px; align-items: center;">
						<label class="muted" style="display: flex; gap: 8px; align-items: center;">
							<input
								style="min-width: 0; width: 18px;"
								type="checkbox"
								checked={deleteFiles[a.id] ?? false}
								on:change={(e) => (deleteFiles[a.id] = e.currentTarget.checked)}
							/>
							deleteFiles (admin)
						</label>
						<button
							class="secondary"
							disabled={busy}
							on:click={() => act(a.id, 'remove', null, `?deleteFiles=${deleteFiles[a.id] ? 'true' : 'false'}`)}
						>
							Remove from client
						</button>
					</div>

					<div class="row" style="margin-top: 10px;">
						<input
							placeholder="blacklist reason (optional)"
							value={blacklistReasons[a.id] ?? ''}
							on:input={(e) => (blacklistReasons[a.id] = e.currentTarget.value)}
						/>
						<label class="muted" style="display: flex; gap: 8px; align-items: center;">
							<input
								style="min-width: 0; width: 18px;"
								type="checkbox"
								checked={blacklistGlobal[a.id] ?? false}
								on:change={(e) => (blacklistGlobal[a.id] = e.currentTarget.checked)}
							/>
							global
						</label>
						<button
							class="secondary"
							disabled={busy}
							on:click={() =>
								act(a.id, 'blacklist_and_rescout', {
									scope: blacklistGlobal[a.id] ? 'global' : 'target',
									reason: (blacklistReasons[a.id] ?? '').trim()
								})}
						>
							Blacklist + rescout
						</button>
					</div>
				</div>
			{/each}
		</div>
	{/if}
</div>

