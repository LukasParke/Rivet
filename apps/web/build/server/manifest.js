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
		client: {start:"_app/immutable/entry/start.7-hRtgay.js",app:"_app/immutable/entry/app.C_0wDNAK.js",imports:["_app/immutable/entry/start.7-hRtgay.js","_app/immutable/chunks/QDDk6FXQ.js","_app/immutable/chunks/D65C3x6o.js","_app/immutable/chunks/k8YrP_PO.js","_app/immutable/entry/app.C_0wDNAK.js","_app/immutable/chunks/D65C3x6o.js","_app/immutable/chunks/DmVZp85x.js","_app/immutable/chunks/9z75sjEA.js","_app/immutable/chunks/D5Af684K.js","_app/immutable/chunks/k8YrP_PO.js"],stylesheets:[],fonts:[],uses_env_dynamic_public:false},
		nodes: [
			__memo(() => import('./chunks/0-Bq1L6iqc.js')),
			__memo(() => import('./chunks/1-DH-Goj3u.js')),
			__memo(() => import('./chunks/2-Cv_rPGHR.js')),
			__memo(() => import('./chunks/3-DzmPCOmq.js')),
			__memo(() => import('./chunks/4-BBKCkZDL.js')),
			__memo(() => import('./chunks/5-DvWnTpGL.js')),
			__memo(() => import('./chunks/6-BOxC-b9M.js')),
			__memo(() => import('./chunks/7-CZ9BLa3X.js'))
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
				id: "/setup/paths",
				pattern: /^\/setup\/paths\/?$/,
				params: [],
				page: { layouts: [0,2,], errors: [1,,], leaf: 7 },
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
