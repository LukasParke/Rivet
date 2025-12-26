<script>
	let email = '';
	let password = '';
	let error = null;
	let busy = false;

	async function submit() {
		busy = true;
		error = null;
		try {
			const login = await fetch('/api/v1/auth/login', {
				method: 'POST',
				headers: { 'content-type': 'application/json' },
				body: JSON.stringify({ email, password })
			});
			if (!login.ok) {
				const body = await login.json().catch(() => ({}));
				throw new Error(body?.error || 'login_failed');
			}
			/** @type {{ token: string }} */
			const data = await login.json();
			sessionStorage.setItem('csrf', data.token);
			window.location.href = '/app';
		} catch (e) {
			error = e instanceof Error ? e.message : String(e);
		} finally {
			busy = false;
		}
	}
</script>

<div class="container">
	<div class="card">
		<h2>Login</h2>
		<p class="muted">Sign in to access Rivet</p>
		<div class="row">
			<input placeholder="Email" type="email" bind:value={email} />
			<input placeholder="Password" type="password" bind:value={password} />
			<button disabled={busy} on:click={submit}>Login</button>
		</div>
		{#if error}
			<p class="error">{error}</p>
		{/if}
	</div>
</div>

