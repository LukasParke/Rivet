<script>
	import { setCsrfToken } from '$lib/api';

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
			// localStorage-backed so every tab shares the token.
			setCsrfToken(data.token);
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
		<form on:submit|preventDefault={submit}>
			<div class="row">
				<div>
					<label class="visually-hidden" for="login_email">Email</label>
					<input
						id="login_email"
						type="email"
						autocomplete="email"
						placeholder="Email"
						bind:value={email}
						required
					/>
				</div>
				<div>
					<label class="visually-hidden" for="login_password">Password</label>
					<input
						id="login_password"
						type="password"
						autocomplete="current-password"
						placeholder="Password"
						bind:value={password}
						required
					/>
				</div>
				<button type="submit" disabled={busy}>Login</button>
			</div>
		</form>
		{#if error}
			<p class="error">{error}</p>
		{/if}
	</div>
</div>
