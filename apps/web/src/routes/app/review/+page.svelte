<script>
	let items = [];
	let busy = false;
	let error = null;
	let wsStatus = 'disconnected';
	let wsRef = null;

	/** @type {Record<string, string>} */
	let overridePaths = {};
	/** @type {Record<string, boolean>} */
	let deleteFiles = {};

	function csrfHeader() {
		const t = sessionStorage.getItem('csrf');
		return t ? { 'x-csrf-token': t } : {};
	}

	async function refresh() {
		busy = true;
		error = null;
		try {
			const res = await fetch('/api/v1/acquisitions?status=needs_review&limit=100');
			const data = await res.json().catch(() => null);
			if (!res.ok) throw new Error(data?.error || 'fetch_failed');
			items = Array.isArray(data) ? data : [];
		} catch (e) {
			error = e instanceof Error ? e.message : String(e);
		} finally {
			busy = false;
		}
	}

	async function act(id, action, body = null, query = '') {
		busy = true;
		error = null;
		try {
			const res = await fetch(`/api/v1/acquisitions/${id}/actions/${action}${query}`, {
				method: 'POST',
				headers: { 'content-type': 'application/json', ...csrfHeader() },
				body: body ? JSON.stringify(body) : undefined
			});
			const data = await res.json().catch(() => null);
			if (!res.ok) throw new Error(data?.error || 'action_failed');
			await refresh();
		} catch (e) {
			error = e instanceof Error ? e.message : String(e);
		} finally {
			busy = false;
		}
	}

	function connectWS() {
		try {
			if (wsRef) wsRef.close();
			const proto = window.location.protocol === 'https:' ? 'wss' : 'ws';
			wsRef = new WebSocket(`${proto}://${window.location.host}/ws`);
			wsStatus = 'connecting';
			wsRef.onopen = () => {
				wsStatus = 'connected';
				wsRef.send(JSON.stringify({ type: 'subscribe', channels: ['downloads', 'imports', 'jobs', 'audit'] }));
			};
			wsRef.onclose = () => {
				wsStatus = 'disconnected';
			};
			wsRef.onerror = () => {
				wsStatus = 'error';
			};
			wsRef.onmessage = (ev) => {
				try {
					const msg = JSON.parse(ev.data);
					if (typeof msg?.type === 'string' && (msg.type.startsWith('download.') || msg.type.startsWith('import.') || msg.type.startsWith('verify.'))) {
						refresh();
					}
				} catch {
					// ignore
				}
			};
		} catch {
			wsStatus = 'error';
		}
	}

	if (typeof window !== 'undefined') {
		setTimeout(connectWS, 0);
		setTimeout(refresh, 0);
	}
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
			<button class="secondary" disabled={busy} on:click={connectWS}>Reconnect WS</button>
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
				</div>
			{/each}
		</div>
	{/if}
</div>

