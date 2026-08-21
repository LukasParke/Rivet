<script>
	/** Password-style input with a show/hide toggle for secrets loaded from
	 * /api/v1/setup/config (API keys, passwords). */
	export let value = '';
	export let inputId = undefined;
	export let name = undefined;
	export let autocomplete = 'off';
	export let placeholder = '';
	export let disabled = false;

	let revealed = false;
</script>

<span class="secret-input">
	<input
		id={inputId}
		{name}
		type={revealed ? 'text' : 'password'}
		{autocomplete}
		{placeholder}
		{disabled}
		bind:value
	/>
	<button
		type="button"
		class="secondary"
		style="flex: 0 0 auto;"
		aria-label={revealed ? 'Hide value' : 'Show value'}
		aria-pressed={revealed}
		title={revealed ? 'Hide value' : 'Show value'}
		on:click={() => (revealed = !revealed)}
	>
		{revealed ? '🙈' : '👁'}
	</button>
</span>

<style>
	.secret-input {
		display: flex;
		gap: 8px;
		align-items: center;
		flex: 1;
		min-width: 0;
	}
	.secret-input input {
		flex: 1;
		min-width: 0;
	}
</style>
