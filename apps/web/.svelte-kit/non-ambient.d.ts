
// this file is generated — do not edit it


declare module "svelte/elements" {
	export interface HTMLAttributes<T> {
		'data-sveltekit-keepfocus'?: true | '' | 'off' | undefined | null;
		'data-sveltekit-noscroll'?: true | '' | 'off' | undefined | null;
		'data-sveltekit-preload-code'?:
			| true
			| ''
			| 'eager'
			| 'viewport'
			| 'hover'
			| 'tap'
			| 'off'
			| undefined
			| null;
		'data-sveltekit-preload-data'?: true | '' | 'hover' | 'tap' | 'off' | undefined | null;
		'data-sveltekit-reload'?: true | '' | 'off' | undefined | null;
		'data-sveltekit-replacestate'?: true | '' | 'off' | undefined | null;
	}
}

export {};


declare module "$app/types" {
	export interface AppTypes {
		RouteId(): "/" | "/app" | "/setup" | "/setup/admin" | "/setup/clients" | "/setup/finish" | "/setup/indexers" | "/setup/metadata" | "/setup/paths" | "/setup/policies";
		RouteParams(): {
			
		};
		LayoutParams(): {
			"/": Record<string, never>;
			"/app": Record<string, never>;
			"/setup": Record<string, never>;
			"/setup/admin": Record<string, never>;
			"/setup/clients": Record<string, never>;
			"/setup/finish": Record<string, never>;
			"/setup/indexers": Record<string, never>;
			"/setup/metadata": Record<string, never>;
			"/setup/paths": Record<string, never>;
			"/setup/policies": Record<string, never>
		};
		Pathname(): "/" | "/app" | "/app/" | "/setup" | "/setup/" | "/setup/admin" | "/setup/admin/" | "/setup/clients" | "/setup/clients/" | "/setup/finish" | "/setup/finish/" | "/setup/indexers" | "/setup/indexers/" | "/setup/metadata" | "/setup/metadata/" | "/setup/paths" | "/setup/paths/" | "/setup/policies" | "/setup/policies/";
		ResolvedPathname(): `${"" | `/${string}`}${ReturnType<AppTypes['Pathname']>}`;
		Asset(): string & {};
	}
}