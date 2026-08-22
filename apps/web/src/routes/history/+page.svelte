<!--
	History page: live event feed over the /ws hub, subscribed to the
	activity, jobs, downloads, imports and audit channels. Newest-first,
	capped at 200 events, with per-channel filter chips. Live-only for now —
	a REST backfill endpoint lands separately (see empty state).
-->
<script>
	import { onDestroy } from 'svelte';
	import { apiFetch } from '$lib/api';
	import { connectWS } from '$lib/ws';
	import Nav from '$lib/components/Nav.svelte';

	const CHANNELS = ['activity', 'jobs', 'downloads', 'imports', 'audit'];
	const MAX_EVENTS = 200;

	let events = [];
	let wsStatus = 'connecting';
	let filter = 'all';
	let wsHandle = null;

	// Backfill from the persisted history endpoint (newest first already).
	async function loadBackfill() {
		const res = await apiFetch('/api/v1/history?pageSize=100');
		if (!res.ok) return;
		const data = await res.json();
		const rows = (data.items ?? []).map((h) => ({
			type: h.eventType,
			ts: h.createdAt,
			channel: channelOf(h.eventType),
			payload: { sourceTitle: h.sourceTitle, quality: h.quality, data: h.data }
		}));
		events = mergeEvents(rows);
	}

	function mergeEvents(incoming) {
		const seen = new Set(events.map((e) => `${e.ts}|${e.type}|${JSON.stringify(e.payload)}`));
		return [...incoming, ...events]
			.filter((e) => {
				const k = `${e.ts}|${e.type}|${JSON.stringify(e.payload)}`;
				if (seen.has(k)) return false;
				seen.add(k);
				return true;
			})
			.sort((a, b) => (a.ts < b.ts ? 1 : -1))
			.slice(0, MAX_EVENTS);
	}
	loadBackfill();

	function channelOf(type) {
		const prefix = String(type).split('.')[0];
		if (prefix === 'audit') return 'audit';
		if (prefix === 'download') return 'downloads';
		if (prefix === 'import' || prefix === 'verify') return 'imports';
		if (['job', 'search', 'candidate', 'decision'].includes(prefix)) return 'jobs';
		return 'activity';
	}

	wsHandle = connectWS({
		channels: CHANNELS,
		onEvent(msg) {
			const type = typeof msg?.type === 'string' ? msg.type : 'unknown';
			events = mergeEvents([
				{
					type,
					ts: typeof msg?.ts === 'string' ? msg.ts : new Date().toISOString(),
					channel: channelOf(type),
					payload: msg?.payload ?? null
				}
			]);
		},
		onStatus(status) {
			wsStatus = status;
		}
	});

	onDestroy(() => {
		if (wsHandle) wsHandle.close();
	});

	function timeOf(ts) {
		const d = new Date(ts);
		return Number.isNaN(d.getTime()) ? ts : d.toLocaleTimeString();
	}

	function preview(payload) {
		if (payload == null) return '';
		try {
			const s = JSON.stringify(payload);
			return s.length > 160 ? `${s.slice(0, 157)}…` : s;
		} catch {
			return String(payload);
		}
	}

	$: shown = filter === 'all' ? events : events.filter((e) => e.channel === filter);
</script>

<Nav active="history" />

<div class="container">
	<h1>History</h1>

	<div class="row" style="align-items: center; margin-bottom: 12px;">
		<span class="status status-{wsStatus}">WS {wsStatus}</span>
		<span class="muted">{events.length} / {MAX_EVENTS} events</span>
	</div>

	<div class="row chips" role="group" aria-label="Filter by channel">
		<button class="secondary chip chip-all" class:selected={filter === 'all'} on:click={() => (filter = 'all')}>
			All
		</button>
		{#each CHANNELS as channel (channel)}
			<button
				class="secondary chip"
				class:selected={filter === channel}
				on:click={() => (filter = channel)}
			>
				{channel}
			</button>
		{/each}
	</div>

	{#if shown.length === 0}
		<div class="card">
			<p class="muted">
				{filter === 'all' ? 'No events yet.' : `No ${filter} events yet.`}
			</p>
		</div>
	{:else}
		<ul class="feed">
			{#each shown as event, i (`${event.ts}-${event.type}-${i}`)}
				<li class="card">
					<div class="row" style="align-items: baseline;">
						<span class="badge">{event.channel}</span>
						<code>{event.type}</code>
						<span class="muted time">{timeOf(event.ts)}</span>
					</div>
					{#if event.payload !== null}
						<pre class="payload muted">{preview(event.payload)}</pre>
					{/if}
				</li>
			{/each}
		</ul>
	{/if}
</div>

<style>
	h1 {
		margin-top: 8px;
	}
	.status {
		font-size: 13px;
		padding: 2px 10px;
		border-radius: 999px;
		border: 1px solid #cbd5e1;
		background: #f1f5f9;
	}
	.status-connected {
		background: rgba(21, 128, 61, 0.12);
		color: #15803d;
		border-color: rgba(21, 128, 61, 0.35);
	}
	.status-error {
		background: rgba(185, 28, 28, 0.12);
		color: #b91c1c;
		border-color: rgba(185, 28, 28, 0.35);
	}
	.chip.selected {
		background: #0f172a;
		color: white;
		border-color: #0f172a;
	}
	.chips button {
		text-transform: none;
	}
	.feed {
		list-style: none;
		margin: 12px 0 0;
		padding: 0;
		display: flex;
		flex-direction: column;
		gap: 8px;
	}
	.badge {
		font-size: 11px;
		padding: 2px 8px;
		border-radius: 999px;
		border: 1px solid #cbd5e1;
		background: #f1f5f9;
		text-transform: uppercase;
		letter-spacing: 0.04em;
	}
	code {
		background: #f1f5f9;
		border-radius: 6px;
		padding: 1px 6px;
		font-size: 12px;
	}
	.time {
		margin-left: auto;
		font-size: 12px;
	}
	.payload {
		margin: 8px 0 0;
		font-size: 12px;
		word-break: break-all;
		white-space: pre-wrap;
	}
</style>
