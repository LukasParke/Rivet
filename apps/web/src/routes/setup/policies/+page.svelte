<script>
	import { apiFetch } from '$lib/api';

	/** @typedef {{ ok: boolean; errors?: string[] }} ValidateResponse */

	let language = 'en';
	let bannedTagsRaw = 'cam\nts\nscr';

	let profileName = 'Default';
	let allowedQualitiesRaw = '1080p\n2160p';
	let upgradeAllowed = true;

	/** @type {ValidateResponse | null} */
	let result = null;
	let error = null;
	let busy = false;


	function payload() {
		const bannedTags = bannedTagsRaw
			.split('\n')
			.map((s) => s.trim())
			.filter(Boolean);
		const allowedQualities = allowedQualitiesRaw
			.split('\n')
			.map((s) => s.trim())
			.filter(Boolean);
		return {
			language,
			bannedTags,
			qualityProfiles: [{ name: profileName, allowedQualities, upgradeAllowed }]
		};
	}

	async function validate() {
		busy = true;
		error = null;
		result = null;
		try {
			const res = await apiFetch('/api/v1/setup/policies/validate', {
				method: 'POST',
				headers: { 'content-type': 'application/json' },
				body: JSON.stringify(payload())
			});
			const data = await res.json().catch(() => null);
			result = /** @type {ValidateResponse} */ (data);
			if (!res.ok || !result?.ok) {
				throw new Error((data?.errors && data.errors.join(', ')) || data?.error || 'validate_failed');
			}
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
			const res = await apiFetch('/api/v1/setup/policies/save', {
				method: 'POST',
				headers: { 'content-type': 'application/json' },
				body: JSON.stringify(payload())
			});
			const data = await res.json().catch(() => null);
			if (!res.ok) {
				throw new Error((data?.errors && data.errors.join(', ')) || data?.error || 'save_failed');
			}
			window.location.href = '/setup/finish';
		} catch (e) {
			error = e instanceof Error ? e.message : String(e);
		} finally {
			busy = false;
		}
	}
</script>

<div class="card">
	<h2>Policies</h2>
	<p class="muted">This is a minimal v0 policy model to unblock the pipeline. We’ll expand profiles/rules later.</p>

	<div class="row">
		<input placeholder="Language (e.g. en)" bind:value={language} />
		<input placeholder="Profile name" bind:value={profileName} />
	</div>

	<div style="margin-top: 12px;">
		<label class="muted" for="allowed_qualities">Allowed qualities (one per line)</label>
		<textarea id="allowed_qualities" rows="3" style="width: 100%; padding: 10px 12px; border: 1px solid #cbd5e1; border-radius: 10px; font-size: 14px;" bind:value={allowedQualitiesRaw}></textarea>
	</div>

	<div style="margin-top: 12px;">
		<label class="muted" for="banned_tags">Banned tags (one per line)</label>
		<textarea id="banned_tags" rows="3" style="width: 100%; padding: 10px 12px; border: 1px solid #cbd5e1; border-radius: 10px; font-size: 14px;" bind:value={bannedTagsRaw}></textarea>
	</div>

	<div style="margin-top: 12px;">
		<label><input type="checkbox" bind:checked={upgradeAllowed} /> Allow upgrades</label>
	</div>

	<div class="row" style="margin-top: 12px;">
		<button disabled={busy} on:click={validate}>Validate</button>
		<button class="secondary" disabled={busy} on:click={save}>Save & Continue</button>
	</div>

	{#if error}
		<p class="error">{error}</p>
	{/if}
	{#if result}
		<pre style="margin-top: 12px;">{JSON.stringify(result, null, 2)}</pre>
	{/if}
</div>
