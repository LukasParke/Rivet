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
			const res = await fetch('/api/v1/setup/admin', {
				method: 'POST',
				headers: { 'content-type': 'application/json' },
				body: JSON.stringify({ email, password })
			});
			if (!res.ok) {
				const body = await res.json().catch(() => ({}));
				throw new Error(body?.message || body?.error || 'request_failed');
			}
			// Immediately log in to proceed.
			const login = await fetch('/api/v1/auth/login', {
				method: 'POST',
				headers: { 'content-type': 'application/json' },
				body: JSON.stringify({ email, password })
			});
			if (!login.ok) {
				throw new Error('login_failed');
			}
			/** @type {{ token: string }} */
			const data = await login.json();
			// localStorage-backed so every tab shares the token.
			setCsrfToken(data.token);
			window.location.href = '/setup/paths';
		} catch (e) {
			error = e instanceof Error ? e.message : String(e);
		} finally {
			busy = false;
		}
	}
</script>

<div class="card">
	<h2>Create Admin</h2>
	<p class="muted">Password must be at least 12 characters.</p>
	<form on:submit|preventDefault={submit}>
		<div class="row">
			<div>
				<label class="visually-hidden" for="admin_email">Email</label>
				<input
					id="admin_email"
					type="email"
					autocomplete="email"
					placeholder="Email"
					bind:value={email}
					required
				/>
			</div>
			<div>
				<label class="visually-hidden" for="admin_password">Password</label>
				<input
					id="admin_password"
					type="password"
					autocomplete="new-password"
					placeholder="Password"
					bind:value={password}
					required
					minlength="12"
				/>
			</div>
			<button type="submit" disabled={busy}>Create</button>
		</div>
	</form>
	{#if error}
		<p class="error">{error}</p>
	{/if}
</div>
