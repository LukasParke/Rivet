<!--
	Interactive search results modal.

	Receives the ranked items from GET /api/v1/search/interactive and renders
	each release with its indexer, kind, parsed resolution/source chips and
	custom-format score. The Grab button POSTs /api/v1/search/interactive/grab.
	Per-indexer error rows ({"indexer", "error"}) are listed at the bottom.
-->
<script>
	import { createEventDispatcher } from 'svelte';
	import { apiFetch } from '$lib/api';

	export let open = false;
	export let query = '';
	export let results = [];
	export let busy = false;
	export let error = null;

	const dispatch = createEventDispatcher();

	let grabbing = {};
	let grabbed = {};
	let grabErrors = {};

	// Reset per-item grab state whenever a new result set arrives.
	let lastResults = results;
	$: if (results !== lastResults) {
		lastResults = results;
		grabbing = {};
		grabbed = {};
		grabErrors = {};
	}

	$: releases = results.filter((r) => r && r.title);
	$: failures = results.filter((r) => r && r.error);

	function close() {
		dispatch('close');
	}

	function onKeydown(e) {
		if (open && e.key === 'Escape') close();
	}

	function resolutionOf(item) {
		return item?.parsed?.quality?.resolution || null;
	}

	function sourceOf(item) {
		const source = item?.parsed?.quality?.source;
		return typeof source === 'string' ? source.replaceAll('-', ' ') : null;
	}

	async function grab(item) {
		if (grabbing[item.title] || grabbed[item.title]) return;
		grabbing = { ...grabbing, [item.title]: true };
		grabErrors = { ...grabErrors, [item.title]: null };
		try {
			const res = await apiFetch('/api/v1/search/interactive/grab', {
				method: 'POST',
				headers: { 'content-type': 'application/json' },
				body: JSON.stringify({
					releaseTitle: item.title,
					downloadUrl: item.downloadUrl,
					kind: item.kind
				})
			});
			const body = await res.json().catch(() => null);
			if (!res.ok) throw new Error(body?.error || 'grab_failed');
			grabbed = { ...grabbed, [item.title]: true };
		} catch (e) {
			grabErrors = { ...grabErrors, [item.title]: e instanceof Error ? e.message : String(e) };
		} finally {
			grabbing = { ...grabbing, [item.title]: false };
		}
	}
</script>

<svelte:window on:keydown={onKeydown} />

{#if open}
		<!-- svelte-ignore a11y_click_events_have_key_events, a11y_no_static_element_interactions -->
		<!-- Click-to-dismiss is supplementary; Escape (svelte:window above) and the
		     Close button are the keyboard-accessible paths. -->
	<div class="backdrop" role="presentation" on:click={close}>
		<div
			class="modal"
			role="dialog"
			aria-modal="true"
			aria-label="Interactive search results"
			on:click|stopPropagation
		>
			<header class="modal-head">
				<h2>Results{query ? ` for “${query}”` : ''}</h2>
				<button class="secondary" on:click={close}>Close</button>
			</header>

			{#if error}
				<p class="error">{error}</p>
			{/if}

			{#if busy}
				<p class="muted">Searching indexers…</p>
			{:else if results.length === 0}
				<p class="muted">No results.</p>
			{:else}
				<ul class="results">
					{#each releases as item (item.title + item.downloadUrl)}
						<li class="result">
							<div class="result-main">
								<span class="title">{item.title}</span>
								<span class="badges">
									<span class="badge">{item.indexer}</span>
									<span class="badge kind">{item.kind}</span>
									{#if resolutionOf(item)}
										<span class="chip">{resolutionOf(item)}</span>
									{/if}
									{#if sourceOf(item)}
										<span class="chip">{sourceOf(item)}</span>
									{/if}
									<span class="score muted">score {item.formatScore ?? 0}</span>
								</span>
							</div>
							<div class="result-actions">
								{#if grabbed[item.title]}
									<span class="muted">Grabbed</span>
								{:else if grabErrors[item.title]}
									<span class="error">{grabErrors[item.title]}</span>
								{/if}
								<button
									disabled={!!grabbing[item.title] || !!grabbed[item.title]}
									on:click={() => grab(item)}
								>
									{grabbing[item.title] ? 'Grabbing…' : 'Grab'}
								</button>
							</div>
						</li>
					{/each}
				</ul>
				{#each failures as f (f.indexer + f.error)}
					<p class="error">{f.indexer}: {f.error}</p>
				{/each}
			{/if}
		</div>
	</div>
{/if}

<style>
	.backdrop {
		position: fixed;
		inset: 0;
		z-index: 100;
		background: rgba(15, 23, 42, 0.45);
		display: flex;
		align-items: flex-start;
		justify-content: center;
		padding: 48px 16px;
		overflow-y: auto;
	}
	.modal {
		background: #fff;
		border-radius: 12px;
		border: 1px solid #e2e8f0;
		max-width: 760px;
		width: 100%;
		padding: 16px;
	}
	.modal-head {
		display: flex;
		align-items: center;
		justify-content: space-between;
		gap: 12px;
		margin-bottom: 12px;
	}
	.modal-head h2 {
		margin: 0;
		font-size: 18px;
	}
	.results {
		list-style: none;
		margin: 0;
		padding: 0;
		display: flex;
		flex-direction: column;
		gap: 8px;
	}
	.result {
		display: flex;
		gap: 12px;
		align-items: flex-start;
		justify-content: space-between;
		flex-wrap: wrap;
		border: 1px solid #e2e8f0;
		border-radius: 10px;
		padding: 10px 12px;
	}
	.title {
		display: block;
		word-break: break-word;
		margin-bottom: 4px;
	}
	.badges {
		display: inline-flex;
		gap: 6px;
		flex-wrap: wrap;
		align-items: center;
	}
	.badge {
		font-size: 12px;
		padding: 2px 8px;
		border-radius: 999px;
		border: 1px solid #cbd5e1;
		background: #f1f5f9;
	}
	.badge.kind {
		text-transform: uppercase;
		letter-spacing: 0.04em;
	}
	.chip {
		font-size: 12px;
		padding: 2px 8px;
		border-radius: 999px;
		background: #0f172a;
		color: white;
	}
	.score {
		font-size: 12px;
	}
	.result-actions {
		display: flex;
		align-items: center;
		gap: 10px;
		flex-shrink: 0;
	}
</style>
