<!--
	Calendar page (*arr style): episodes airing this week, grouped by date,
	with prev/next week navigation against GET /api/v1/calendar?start&end and
	missing badges from the /api/v1/calendar/missing backlog.
-->
<script>
	import { apiFetch } from '$lib/api';
	import Nav from '$lib/components/Nav.svelte';

	export let data;

	let start = data.start;
	let end = data.end;
	let episodes = data.episodes ?? [];
	// Dynamic membership test across a growing backlog — Set is the right shape.
	let missingIds = new Set(data.missing ?? []);
	let busy = false;
	let error = null;

	async function shiftWeek(days) {
		const nextStart = new Date(`${start}T00:00:00Z`);
		nextStart.setUTCDate(nextStart.getUTCDate() + days);
		const nextEnd = new Date(nextStart);
		nextEnd.setUTCDate(nextEnd.getUTCDate() + 6);
		const startIso = nextStart.toISOString().slice(0, 10);
		const endIso = nextEnd.toISOString().slice(0, 10);

		busy = true;
		error = null;
		try {
			const res = await apiFetch(`/api/v1/calendar?start=${startIso}&end=${endIso}`);
			const body = await res.json().catch(() => null);
			if (!res.ok) throw new Error(body?.error || 'calendar_failed');
			start = startIso;
			end = endIso;
			episodes = Array.isArray(body) ? body : [];
		} catch (e) {
			error = e instanceof Error ? e.message : String(e);
		} finally {
			busy = false;
		}
	}

	function groupByDate(items) {
		const map = new Map();
		for (const ep of items) {
			const day = (ep.airDateUtc || '').slice(0, 10) || 'Unscheduled';
			if (!map.has(day)) map.set(day, []);
			map.get(day).push(ep);
		}
		return [...map.entries()].sort((a, b) => (a[0] < b[0] ? -1 : a[0] > b[0] ? 1 : 0));
	}

	function pad(n) {
		return String(n).padStart(2, '0');
	}

	function seCode(ep) {
		return `S${pad(ep.seasonNumber)}E${pad(ep.episodeNumber)}`;
	}

	$: byDate = groupByDate(episodes);
</script>

<Nav active="calendar" />

<div class="container">
	<h1>Calendar</h1>

	<div class="row" style="align-items: center; margin-bottom: 16px;">
		<button class="secondary" disabled={busy} on:click={() => shiftWeek(-7)}>‹ Prev week</button>
		<button class="secondary" disabled={busy} on:click={() => shiftWeek(7)}>Next week ›</button>
		<span class="muted">{start} → {end}</span>
		{#if busy}<span class="muted">Loading…</span>{/if}
	</div>

	{#if error}
		<p class="error">{error}</p>
	{/if}

	{#if byDate.length === 0 && !busy}
		<div class="card">
			<p class="muted">No episodes airing in this window. Add series with monitored episodes to populate the calendar.</p>
		</div>
	{:else}
		<div class="grid">
			{#each byDate as [day, dayEpisodes] (day)}
				<section class="card">
					<h2>{day}</h2>
					<ul class="episodes">
						{#each dayEpisodes as ep (ep.id)}
							<li class:unmonitored={!ep.monitored}>
								<span class="series">{ep.seriesTitle}</span>
								<code>{seCode(ep)}</code>
								<span class="ep-title">{ep.title || 'TBA'}</span>
								<span class="badges">
									{#if missingIds.has(ep.id)}
										<span class="badge missing">Missing</span>
									{/if}
									{#if ep.hasFile}
										<span class="badge file">Downloaded</span>
									{/if}
									{#if !ep.monitored}
										<span class="badge unmon">Unmonitored</span>
									{/if}
								</span>
							</li>
						{/each}
					</ul>
				</section>
			{/each}
		</div>
	{/if}
</div>

<style>
	h1 {
		margin-top: 8px;
	}
	h2 {
		margin: 0 0 8px;
		font-size: 15px;
	}
	.grid {
		display: grid;
		grid-template-columns: repeat(auto-fill, minmax(260px, 1fr));
		gap: 12px;
	}
	.episodes {
		list-style: none;
		margin: 0;
		padding: 0;
	}
	.episodes li {
		display: flex;
		flex-direction: column;
		gap: 4px;
		padding: 8px 0;
		border-top: 1px solid #f1f5f9;
		font-size: 14px;
	}
	li.unmonitored .series,
	li.unmonitored .ep-title {
		opacity: 0.6;
	}
	.series {
		font-weight: 600;
	}
	code {
		background: #f1f5f9;
		border-radius: 6px;
		padding: 1px 6px;
		font-size: 12px;
		align-self: flex-start;
	}
	.badges {
		display: inline-flex;
		gap: 6px;
		flex-wrap: wrap;
	}
	.badge {
		font-size: 11px;
		padding: 2px 8px;
		border-radius: 999px;
		border: 1px solid transparent;
	}
	.badge.missing {
		background: rgba(185, 28, 28, 0.12);
		color: #b91c1c;
		border-color: rgba(185, 28, 28, 0.35);
	}
	.badge.file {
		background: rgba(21, 128, 61, 0.12);
		color: #15803d;
		border-color: rgba(21, 128, 61, 0.35);
	}
	.badge.unmon {
		background: #f1f5f9;
		color: #475569;
		border-color: #e2e8f0;
	}
</style>
