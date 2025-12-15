<script>
	let kind = 'movie';
	let externalSource = 'tmdb';
	let externalId = '550';
	let query = 'Fight Club 1999 1080p';

	let runId = '';
	let targetId = '';
	let error = null;
	let busy = false;
	let results = null;
	let wsStatus = 'disconnected';
	let events = [];
	let wsRef = null;
	let acquisitions = [];
	let acqError = null;
	let acqBusy = false;
	let acqRefreshTimer = null;
	let acqStatusFilter = '';
	let overrideImportedPath = '';

	function csrfHeader() {
		const t = sessionStorage.getItem('csrf');
		return t ? { 'x-csrf-token': t } : {};
	}

	async function createTarget() {
		busy = true;
		error = null;
		try {
			const res = await fetch('/api/v1/targets', {
				method: 'POST',
				headers: { 'content-type': 'application/json', ...csrfHeader() },
				body: JSON.stringify({ kind, externalSource, externalId, query })
			});
			const data = await res.json().catch(() => null);
			if (!res.ok) throw new Error(data?.error || 'create_failed');
			targetId = data.id;
		} catch (e) {
			error = e instanceof Error ? e.message : String(e);
		} finally {
			busy = false;
		}
	}

	async function startSearch() {
		if (!targetId) return;
		busy = true;
		error = null;
		results = null;
		try {
			const res = await fetch(`/api/v1/targets/${targetId}/search`, {
				method: 'POST',
				headers: { ...csrfHeader() }
			});
			const data = await res.json().catch(() => null);
			if (!res.ok) throw new Error(data?.error || 'start_failed');
			runId = data.runId;
		} catch (e) {
			error = e instanceof Error ? e.message : String(e);
		} finally {
			busy = false;
		}
	}

	async function refresh() {
		if (!runId) return;
		busy = true;
		error = null;
		try {
			const res = await fetch(`/api/v1/search/runs/${runId}`);
			const data = await res.json().catch(() => null);
			if (!res.ok) throw new Error(data?.error || 'fetch_failed');
			results = data;
		} catch (e) {
			error = e instanceof Error ? e.message : String(e);
		} finally {
			busy = false;
		}
	}

	async function refreshAcquisitions() {
		acqBusy = true;
		acqError = null;
		try {
			const q = acqStatusFilter ? `?status=${encodeURIComponent(acqStatusFilter)}` : '';
			const res = await fetch(`/api/v1/acquisitions${q}`);
			const data = await res.json().catch(() => null);
			if (!res.ok) throw new Error(data?.error || 'fetch_failed');
			acquisitions = Array.isArray(data) ? data : [];
		} catch (e) {
			acqError = e instanceof Error ? e.message : String(e);
		} finally {
			acqBusy = false;
		}
	}

	async function act(acquisitionId, action, body = null) {
		acqBusy = true;
		acqError = null;
		try {
			const res = await fetch(`/api/v1/acquisitions/${acquisitionId}/actions/${action}`, {
				method: 'POST',
				headers: { 'content-type': 'application/json', ...csrfHeader() },
				body: body ? JSON.stringify(body) : undefined
			});
			const data = await res.json().catch(() => null);
			if (!res.ok) throw new Error(data?.error || 'action_failed');
			scheduleAcqRefresh();
		} catch (e) {
			acqError = e instanceof Error ? e.message : String(e);
		} finally {
			acqBusy = false;
		}
	}

	function scheduleAcqRefresh() {
		if (acqRefreshTimer) return;
		acqRefreshTimer = setTimeout(() => {
			acqRefreshTimer = null;
			refreshAcquisitions();
		}, 500);
	}

	function connectWS() {
		try {
			if (wsRef) wsRef.close();
			const proto = window.location.protocol === 'https:' ? 'wss' : 'ws';
			wsRef = new WebSocket(`${proto}://${window.location.host}/ws`);
			wsStatus = 'connecting';
			wsRef.onopen = () => {
				wsStatus = 'connected';
				wsRef.send(JSON.stringify({ type: 'subscribe', channels: ['activity', 'jobs', 'audit', 'downloads', 'imports'] }));
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
					events = [{ ts: msg.ts, type: msg.type, payload: msg.payload }, ...events].slice(0, 200);
					if (typeof msg?.type === 'string' && (msg.type.startsWith('download.') || msg.type.startsWith('import.'))) {
						scheduleAcqRefresh();
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
		setTimeout(refreshAcquisitions, 0);
	}
</script>

<div class="container">
	<h1>Rivet (MVP pipeline demo)</h1>
	<p class="muted">Create a target, run an indexer search, store candidates + decisions.</p>

	<div class="card">
		<h2>Create target</h2>
		<div class="row">
			<input placeholder="kind (movie/episode/season_pack)" bind:value={kind} />
			<input placeholder="externalSource (tmdb/tvdb)" bind:value={externalSource} />
			<input placeholder="externalId" bind:value={externalId} />
			<input placeholder="query" bind:value={query} />
			<button disabled={busy} on:click={createTarget}>Create</button>
		</div>
		{#if targetId}
			<p class="muted">Target ID: <code>{targetId}</code></p>
			<div class="row">
				<button disabled={busy} on:click={startSearch}>Start search</button>
				<button class="secondary" disabled={busy} on:click={refresh}>Refresh run</button>
			</div>
		{/if}
		{#if runId}
			<p class="muted">Run ID: <code>{runId}</code></p>
		{/if}
		{#if error}
			<p class="error">{error}</p>
		{/if}
	</div>

	<div class="card" style="margin-top: 16px;">
		<h2>Realtime feed (WebSocket)</h2>
		<p class="muted">Status: {wsStatus}</p>
		<div class="row">
			<button class="secondary" disabled={busy} on:click={connectWS}>Reconnect</button>
		</div>
		<pre style="max-height: 340px; overflow: auto;">{JSON.stringify(events, null, 2)}</pre>
	</div>

	<div class="card" style="margin-top: 16px;">
		<h2>Acquisitions</h2>
		<p class="muted">Live updates from download/import events. Filter by status to use this as a minimal “review queue”.</p>
		<div class="row">
			<input placeholder="status filter (e.g. needs_review)" bind:value={acqStatusFilter} />
			<button class="secondary" disabled={acqBusy} on:click={refreshAcquisitions}>Refresh acquisitions</button>
		</div>
		{#if acqError}
			<p class="error">{acqError}</p>
		{/if}
		{#if acquisitions?.length}
			<div style="display: grid; gap: 12px; margin-top: 12px;">
				{#each acquisitions as a (a.id)}
					<div class="card" style="margin: 0;">
						<div class="row" style="justify-content: space-between; align-items: center;">
							<div>
								<div><strong>{a.id}</strong></div>
								<div class="muted">status: <code>{a.status}</code> client: <code>{a.clientKind}</code> verify: <code>{a.verifyStatus ?? ''}</code></div>
								{#if a.lastError}<div class="error">lastError: {a.lastError}</div>{/if}
							</div>
							<div class="row" style="gap: 8px; flex-wrap: wrap;">
								<button class="secondary" disabled={acqBusy} on:click={() => act(a.id, 'pause')}>Pause</button>
								<button class="secondary" disabled={acqBusy} on:click={() => act(a.id, 'resume')}>Resume</button>
								<button class="secondary" disabled={acqBusy} on:click={() => act(a.id, 'retry')}>Retry</button>
								{#if a.status === 'needs_review'}
									<button class="secondary" disabled={acqBusy} on:click={() => act(a.id, 'reverify')}>Re-verify</button>
									<button class="secondary" disabled={acqBusy} on:click={() => act(a.id, 'reimport')}>Re-import</button>
								{/if}
							</div>
						</div>
						<details style="margin-top: 8px;">
							<summary class="muted">Details</summary>
							<pre style="max-height: 240px; overflow: auto;">{JSON.stringify(a, null, 2)}</pre>
						</details>
						{#if a.status === 'needs_review'}
							<div class="row" style="margin-top: 8px;">
								<input placeholder="override importedPath (admin)" bind:value={overrideImportedPath} />
								<button
									class="secondary"
									disabled={acqBusy || !overrideImportedPath}
									on:click={() => act(a.id, 'override_imported', { importedPath: overrideImportedPath })}
								>
									Mark imported
								</button>
							</div>
						{/if}
					</div>
				{/each}
			</div>
		{:else}
			<p class="muted">No acquisitions yet.</p>
		{/if}
	</div>

	{#if results}
		<div class="card" style="margin-top: 16px;">
			<h2>Results</h2>
			<pre>{JSON.stringify(results, null, 2)}</pre>
		</div>
	{/if}
</div>
