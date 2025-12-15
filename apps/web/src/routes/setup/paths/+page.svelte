<script>
	/** @typedef {{ path: string; exists: boolean; isDir: boolean; readable: boolean; writable: boolean; errors?: string[]; warnings?: string[]; filesystemId?: string; hardlinkViable: boolean }} PathCheck */
	/** @typedef {{ ok: boolean; downloadsCompleted: PathCheck; libraryRoots: PathCheck[]; quarantine: PathCheck }} ValidateResponse */

	let downloadsCompletedPath = '/downloads/completed';
	let quarantinePath = '/quarantine';
	let libraryRootsRaw = '/library';
	/** @type {ValidateResponse | null} */
	let result = null;
	let error = null;
	let busy = false;

	function csrfHeader() {
		const t = sessionStorage.getItem('csrf');
		return t ? { 'x-csrf-token': t } : {};
	}

	function payload() {
		const roots = libraryRootsRaw
			.split('\n')
			.map((s) => s.trim())
			.filter(Boolean);
		return { downloadsCompletedPath, quarantinePath, libraryRoots: roots };
	}

	async function validate() {
		busy = true;
		error = null;
		result = null;
		try {
			const res = await fetch('/api/v1/setup/paths/validate', {
				method: 'POST',
				headers: { 'content-type': 'application/json', ...csrfHeader() },
				body: JSON.stringify(payload())
			});
			const data = await res.json().catch(() => null);
			if (!res.ok) {
				throw new Error(data?.message || data?.error || 'validate_failed');
			}
			result = /** @type {ValidateResponse} */ (data);
		} catch (e) {
			error = e instanceof Error ? e.message : String(e);
		} finally {
			busy = false;
		}
	}

	async function save() {
		busy = true;
		error = null;
		try {
			const res = await fetch('/api/v1/setup/paths/save', {
				method: 'POST',
				headers: { 'content-type': 'application/json', ...csrfHeader() },
				body: JSON.stringify(payload())
			});
			const data = await res.json().catch(() => null);
			if (!res.ok) {
				// If it returned validation response, show it.
				if (data?.downloadsCompleted) {
					result = /** @type {ValidateResponse} */ (data);
					return;
				}
				throw new Error(data?.message || data?.error || 'save_failed');
			}
			window.location.href = '/setup';
		} catch (e) {
			error = e instanceof Error ? e.message : String(e);
		} finally {
			busy = false;
		}
	}
</script>

<div class="card">
	<h2>Paths</h2>
	<p class="muted">These paths must be accessible to the container. Hardlink viability is checked between downloads and library roots.</p>

	<div class="row">
		<input placeholder="Downloads completed path" bind:value={downloadsCompletedPath} />
		<input placeholder="Quarantine path" bind:value={quarantinePath} />
	</div>
	<div style="margin-top: 12px;">
		<textarea
			rows="4"
			style="width: 100%; padding: 10px 12px; border: 1px solid #cbd5e1; border-radius: 10px; font-size: 14px;"
			placeholder="Library roots (one per line)"
			bind:value={libraryRootsRaw}
		></textarea>
	</div>

	<div class="row" style="margin-top: 12px;">
		<button disabled={busy} on:click={validate}>Validate</button>
		<button class="secondary" disabled={busy || !result?.ok} on:click={save}>Save</button>
	</div>

	{#if error}
		<p class="error">{error}</p>
	{/if}

	{#if result}
		<div style="margin-top: 12px;">
			<p><strong>Overall:</strong> {result.ok ? 'OK' : 'Needs changes'}</p>
			<h3>Downloads</h3>
			<pre>{JSON.stringify(result.downloadsCompleted, null, 2)}</pre>
			<h3>Library roots</h3>
			<pre>{JSON.stringify(result.libraryRoots, null, 2)}</pre>
			<h3>Quarantine</h3>
			<pre>{JSON.stringify(result.quarantine, null, 2)}</pre>
		</div>
	{/if}
</div>
