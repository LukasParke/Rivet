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

	{#if results}
		<div class="card" style="margin-top: 16px;">
			<h2>Results</h2>
			<pre>{JSON.stringify(results, null, 2)}</pre>
		</div>
	{/if}
</div>
