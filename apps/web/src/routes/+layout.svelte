<script lang="ts">
	import '../styles.css';
	import { onMount } from 'svelte';

	let dryRun = false;
	let dryRunFixturePathSet = false;

	onMount(async () => {
		try {
			const res = await fetch('/api/v1/setup/status');
			if (!res.ok) return;
			const data = (await res.json()) as { dryRun?: boolean; dryRunFixturePathSet?: boolean };
			dryRun = !!data.dryRun;
			dryRunFixturePathSet = !!data.dryRunFixturePathSet;
		} catch {
			// ignore
		}
	});
</script>

{#if dryRun}
	<div class="dryrun-banner">
		<strong>Dry Run</strong> — downloads and filesystem writes are simulated
		{#if dryRunFixturePathSet}
			<span class="muted">(fixture configured)</span>
		{:else}
			<span class="muted">(no fixture)</span>
		{/if}
	</div>
{/if}

<slot />

<style>
	.dryrun-banner {
		position: sticky;
		top: 0;
		z-index: 50;
		padding: 10px 14px;
		background: rgba(255, 203, 87, 0.18);
		border-bottom: 1px solid rgba(255, 203, 87, 0.35);
		color: inherit;
		backdrop-filter: blur(8px);
	}
	.muted {
		opacity: 0.75;
		margin-left: 8px;
	}
	strong {
		font-weight: 700;
	}
	@media (prefers-color-scheme: dark) {
		.dryrun-banner {
			background: rgba(255, 203, 87, 0.12);
			border-bottom-color: rgba(255, 203, 87, 0.25);
		}
	}
</style>
