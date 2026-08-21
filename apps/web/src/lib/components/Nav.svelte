<!-- Minimal top navigation shared by the *arr pages. -->
<script>
	export let active = '';
</script>

<nav class="nav" aria-label="Primary">
	<a href="/app" class:active={active === 'app'}>App</a>
	<a href="/series" class:active={active === 'series'}>Series</a>
	<a href="/calendar" class:active={active === 'calendar'}>Calendar</a>
	<a href="/history" class:active={active === 'history'}>History</a>
</nav>

<style>
	.nav {
		display: flex;
		gap: 8px;
		max-width: 900px;
		margin: 0 auto;
		padding: 16px 24px 0;
		flex-wrap: wrap;
	}
	.nav a {
		color: #0f172a;
		text-decoration: none;
		font-size: 14px;
		padding: 6px 12px;
		border-radius: 10px;
		border: 1px solid transparent;
	}
	.nav a:hover {
		background: #e2e8f0;
	}
	.nav a.active {
		background: #0f172a;
		color: white;
		border-color: #0f172a;
	}
</style>
