<!--
	Series hub page (*arr style):
	- Add-series form POSTing /api/v1/targets
	- Interactive search (GET /api/v1/search/interactive) rendered in
	  SearchResultsModal with per-release Grab buttons
	- Library activity grouped from GET /api/v1/wanted/missing
-->
<script>
	import { apiFetch } from '$lib/api';
	import Nav from '$lib/components/Nav.svelte';
	import SearchResultsModal from '$lib/components/SearchResultsModal.svelte';

	export let data;

	// --- Add series -------------------------------------------------------
	const KINDS = ['series', 'movie', 'episode', 'season_pack'];
	let kind = 'series';
	let externalSource = 'tvdb';
	let externalId = '';
	let query = '';
	let addBusy = false;
	let addResult = null;
	let addError = null;

	async function addTarget() {
		addBusy = true;
		addError = null;
		addResult = null;
		try {
			const res = await apiFetch('/api/v1/targets', {
				method: 'POST',
				headers: { 'content-type': 'application/json' },
				body: JSON.stringify({
					kind,
					externalSource: externalSource.trim(),
					externalId: externalId.trim(),
					query: query.trim()
				})
			});
			const body = await res.json().catch(() => null);
			if (!res.ok) throw new Error(body?.error || 'create_failed');
			addResult = body;
			externalId = '';
			query = '';
		} catch (e) {
			addError = e instanceof Error ? e.message : String(e);
		} finally {
			addBusy = false;
		}
	}

	// --- Interactive search ----------------------------------------------
	let searchQuery = '';
	let searchBusy = false;
	let searchError = null;
	let searchResults = [];
	let modalOpen = false;

	async function runSearch() {
		const q = searchQuery.trim();
		if (!q) return;
		searchBusy = true;
		searchError = null;
		searchResults = [];
		modalOpen = true;
		try {
			const res = await apiFetch(`/api/v1/search/interactive?query=${encodeURIComponent(q)}`);
			const body = await res.json().catch(() => null);
			if (!res.ok) throw new Error(body?.error || 'search_failed');
			searchResults = Array.isArray(body) ? body : [];
		} catch (e) {
			searchError = e instanceof Error ? e.message : String(e);
		} finally {
			searchBusy = false;
		}
	}

	// --- Library (real GET /api/v1/series) --------------------------------
	let library = [];
	let libraryError = null;

	async function loadLibrary() {
		try {
			const res = await apiFetch('/api/v1/series');
			if (!res.ok) {
				libraryError = `library_load_failed (${res.status})`;
				return;
			}
			library = await res.json();
			libraryError = null;
		} catch (e) {
			libraryError = String(e);
		}
	}
	loadLibrary();

	async function deleteSeries(id) {
		await apiFetch(`/api/v1/series/${id}`, { method: 'DELETE' });
		await loadLibrary();
	}

	async function toggleMonitor(s) {
		await apiFetch(`/api/v1/series/${s.id}/monitor`, {
			method: 'PUT',
			headers: { 'content-type': 'application/json' },
			body: JSON.stringify({ monitored: !s.monitored })
		});
		await loadLibrary();
	}

	// --- Library activity -------------------------------------------------
	let activity = data.missing ?? [];
	let activityTotal = data.total ?? 0;
	let activityBusy = false;
	let activityError = null;

	async function refreshActivity() {
		activityBusy = true;
		activityError = null;
		try {
			const res = await apiFetch('/api/v1/wanted/missing?pageSize=50');
			const body = await res.json().catch(() => null);
			if (!res.ok) throw new Error(body?.error || 'fetch_failed');
			activity = Array.isArray(body?.items) ? body.items : [];
			activityTotal = body?.total ?? 0;
		} catch (e) {
			activityError = e instanceof Error ? e.message : String(e);
		} finally {
			activityBusy = false;
		}
	}

	function groupBySeries(items) {
		const map = new Map();
		for (const item of items) {
			const key = item.seriesTitle || 'Unknown series';
			if (!map.has(key)) map.set(key, []);
			map.get(key).push(item);
		}
		return [...map.entries()];
	}

	function pad(n) {
		return String(n).padStart(2, '0');
	}

	function seCode(ep) {
		return `S${pad(ep.seasonNumber)}E${pad(ep.episodeNumber)}`;
	}

	$: groups = groupBySeries(activity);
</script>

<Nav active="series" />

<div class="container">
	<h1>Series</h1>
	<p class="muted">Add wanted targets to the library, search interactively and grab releases.</p>

	<section class="card" style="margin-bottom: 16px;">
		<h2>Add series</h2>
		<form class="row" on:submit|preventDefault={addTarget}>
			<label>
				<span class="field-label">Kind</span>
				<select bind:value={kind}>
					{#each KINDS as k}
						<option value={k}>{k}</option>
					{/each}
				</select>
			</label>
			<label>
				<span class="field-label">External source</span>
				<input placeholder="tmdb / tvdb" bind:value={externalSource} />
			</label>
			<label>
				<span class="field-label">External ID</span>
				<input placeholder="e.g. 121361" bind:value={externalId} />
			</label>
			<label>
				<span class="field-label">Search query</span>
				<input placeholder="Title to search for" bind:value={query} />
			</label>
			<button type="submit" disabled={addBusy}>{addBusy ? 'Adding…' : 'Add'}</button>
		</form>
		{#if addResult}
			<p class="muted">
				Created target <code>{addResult.ID}</code> ({addResult.Kind}, {addResult.State})
			</p>
		{/if}
		{#if addError}
			<p class="error">{addError}</p>
		{/if}
	</section>

	<section class="card" style="margin-bottom: 16px;">
		<h2>Interactive search</h2>
		<form class="row" on:submit|preventDefault={runSearch}>
			<input placeholder="Release or series name…" bind:value={searchQuery} />
			<button type="submit" disabled={searchBusy || !searchQuery.trim()}>
				{searchBusy ? 'Searching…' : 'Search'}
			</button>
		</form>
		<p class="muted">Queries every configured indexer and ranks releases by format score.</p>
	</section>

	<section class="card">
		<div class="row" style="justify-content: space-between; align-items: center;">
			<h2 style="margin: 0;">Library</h2>
		{#if libraryError}
			<p class="muted">{libraryError}</p>
		{:else if library.length === 0}
			<p class="muted">No series yet — add one above.</p>
		{:else}
			<table style="width: 100%; border-collapse: collapse;">
				<thead>
					<tr>
						<th style="text-align: left;">Title</th>
						<th style="text-align: left;">Status</th>
						<th style="text-align: left;">Monitored</th>
						<th style="text-align: left;">Actions</th>
					</tr>
				</thead>
				<tbody>
					{#each library as s (s.id)}
						<tr>
							<td>{s.title}</td>
							<td>{s.status}</td>
							<td>
								<button on:click={() => toggleMonitor(s)}>
									{s.monitored ? 'Unmonitor' : 'Monitor'}
								</button>
							</td>
							<td>
								<button on:click={() => deleteSeries(s.id)}>Delete</button>
							</td>
						</tr>
					{/each}
				</tbody>
			</table>
		{/if}

		<h2 style="margin: 0;">Library activity</h2>
			<button class="secondary" disabled={activityBusy} on:click={refreshActivity}>
				{activityBusy ? 'Refreshing…' : 'Refresh'}
			</button>
		</div>
		<p class="muted">
			Monitored episodes without a file — {activity.length} shown of {activityTotal} missing.
		</p>
		{#if activityError}
			<p class="error">{activityError}</p>
		{:else if groups.length === 0}
			<p class="muted">Nothing missing. The library is fully grabbed.</p>
		{:else}
			{#each groups as [seriesTitle, episodes] (seriesTitle)}
				<div class="group">
					<h3>{seriesTitle}</h3>
					<ul class="episodes">
						{#each episodes as ep (ep.id)}
							<li>
								<code>{seCode(ep)}</code>
								<span>{ep.title || 'TBA'}</span>
								{#if ep.airDateUtc}
									<span class="muted">aired {ep.airDateUtc.slice(0, 10)}</span>
								{/if}
							</li>
						{/each}
					</ul>
				</div>
			{/each}
		{/if}
	</section>
</div>

<SearchResultsModal
	open={modalOpen}
	query={searchQuery}
	results={searchResults}
	busy={searchBusy}
	error={searchError}
	on:close={() => (modalOpen = false)}
/>

<style>
	h2 {
		margin-top: 0;
		font-size: 16px;
	}
	label {
		display: flex;
		flex-direction: column;
		gap: 4px;
		font-size: 13px;
	}
	select {
		padding: 10px 12px;
		border: 1px solid #cbd5e1;
		border-radius: 10px;
		font-size: 14px;
		background: white;
		min-width: 160px;
	}
	.field-label {
		color: #475569;
	}
	.group {
		margin-bottom: 16px;
	}
	.group h3 {
		margin: 8px 0;
		font-size: 15px;
	}
	.episodes {
		list-style: none;
		margin: 0;
		padding: 0;
	}
	.episodes li {
		display: flex;
		gap: 10px;
		align-items: baseline;
		flex-wrap: wrap;
		padding: 6px 0;
		border-top: 1px solid #f1f5f9;
		font-size: 14px;
	}
	code {
		background: #f1f5f9;
		border-radius: 6px;
		padding: 1px 6px;
		font-size: 12px;
	}
</style>
