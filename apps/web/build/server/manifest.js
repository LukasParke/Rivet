const manifest = (() => {
function __memo(fn) {
	let value;
	return () => value ??= (value = fn());
}

return {
	appDir: "_app",
	appPath: "_app",
	assets: new Set([]),
	mimeTypes: {},
	_: {
		client: {start:"_app/immutable/entry/start.ClA4nHeH.js",app:"_app/immutable/entry/app.CdfWV75y.js",imports:["_app/immutable/entry/start.ClA4nHeH.js","_app/immutable/chunks/K6XtGTg8.js","_app/immutable/chunks/D65C3x6o.js","_app/immutable/chunks/k8YrP_PO.js","_app/immutable/entry/app.CdfWV75y.js","_app/immutable/chunks/D65C3x6o.js","_app/immutable/chunks/DmVZp85x.js","_app/immutable/chunks/9z75sjEA.js","_app/immutable/chunks/D5Af684K.js","_app/immutable/chunks/k8YrP_PO.js"],stylesheets:[],fonts:[],uses_env_dynamic_public:false},
		nodes: [
			__memo(() => import('./chunks/0-Bq1L6iqc.js')),
			__memo(() => import('./chunks/1-BEWS29Lq.js')),
			__memo(() => import('./chunks/2-Cv_rPGHR.js')),
			__memo(() => import('./chunks/3-DzmPCOmq.js')),
			__memo(() => import('./chunks/4-Coyas3GU.js')),
			__memo(() => import('./chunks/5-DvWnTpGL.js')),
			__memo(() => import('./chunks/6-BOxC-b9M.js')),
			__memo(() => import('./chunks/7-DmHfHN2h.js')),
			__memo(() => import('./chunks/8-BRB0qA6c.js')),
			__memo(() => import('./chunks/9--ADZHvDl.js')),
			__memo(() => import('./chunks/10-Diq6oKEK.js')),
			__memo(() => import('./chunks/11-DDRWC513.js'))
		],
		remotes: {
			
		},
		routes: [
			{
				id: "/",
				pattern: /^\/$/,
				params: [],
				page: { layouts: [0,], errors: [1,], leaf: 3 },
				endpoint: null
			},
			{
				id: "/setup",
				pattern: /^\/setup\/?$/,
				params: [],
				page: { layouts: [0,2,], errors: [1,,], leaf: 4 },
				endpoint: null
			},
			{
				id: "/setup/admin",
				pattern: /^\/setup\/admin\/?$/,
				params: [],
				page: { layouts: [0,2,], errors: [1,,], leaf: 5 },
				endpoint: null
			},
			{
				id: "/setup/clients",
				pattern: /^\/setup\/clients\/?$/,
				params: [],
				page: { layouts: [0,2,], errors: [1,,], leaf: 6 },
				endpoint: null
			},
			{
				id: "/setup/finish",
				pattern: /^\/setup\/finish\/?$/,
				params: [],
				page: { layouts: [0,2,], errors: [1,,], leaf: 7 },
				endpoint: null
			},
			{
				id: "/setup/indexers",
				pattern: /^\/setup\/indexers\/?$/,
				params: [],
				page: { layouts: [0,2,], errors: [1,,], leaf: 8 },
				endpoint: null
			},
			{
				id: "/setup/metadata",
				pattern: /^\/setup\/metadata\/?$/,
				params: [],
				page: { layouts: [0,2,], errors: [1,,], leaf: 9 },
				endpoint: null
			},
			{
				id: "/setup/paths",
				pattern: /^\/setup\/paths\/?$/,
				params: [],
				page: { layouts: [0,2,], errors: [1,,], leaf: 10 },
				endpoint: null
			},
			{
				id: "/setup/policies",
				pattern: /^\/setup\/policies\/?$/,
				params: [],
				page: { layouts: [0,2,], errors: [1,,], leaf: 11 },
				endpoint: null
			}
		],
		prerendered_routes: new Set([]),
		matchers: async () => {
			
			return {  };
		},
		server_assets: {}
	}
}
})();

const prerendered = new Set([]);

const base = "";

export { base, manifest, prerendered };
//# sourceMappingURL=manifest.js.map
