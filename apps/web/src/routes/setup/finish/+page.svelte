<script>
	let busy = false;
	let error = null;
	let result = null;

	function csrfHeader() {
		const t = sessionStorage.getItem('csrf');
		return t ? { 'x-csrf-token': t } : {};
	}

	async function finish() {
		busy = true;
		error = null;
		result = null;
		try {
			const res = await fetch('/api/v1/setup/finish', {
				method: 'POST',
				headers: { ...csrfHeader() }
			});
			const data = await res.json().catch(() => null);
			if (!res.ok) throw new Error(data?.error || 'finish_failed');
			result = data;
			// Setup mode (in our current gate) ends after admin exists; this just records the report.
		} catch (e) {
			error = e instanceof Error ? e.message : String(e);
		} finally {
			busy = false;
		}
	}
</script>

<div class="card">
	<h2>Finish</h2>
	<p class="muted">This records a setup report snapshot and closes registration by default.</p>
	<div class="row">
		<button disabled={busy} on:click={finish}>Finish setup</button>
		<a href="/"><button class="secondary">Go to app</button></a>
	</div>
	{#if error}
		<p class="error">{error}</p>
	{/if}
	{#if result}
		<pre style="margin-top: 12px;">{JSON.stringify(result, null, 2)}</pre>
	{/if}
</div>
