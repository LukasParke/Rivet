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
		client: {start:"_app/immutable/entry/start.AIw2W-op.js",app:"_app/immutable/entry/app.CK9ESlOD.js",imports:["_app/immutable/entry/start.AIw2W-op.js","_app/immutable/chunks/CEAfuFXW.js","_app/immutable/chunks/Dc2GWTX7.js","_app/immutable/chunks/BNwFUchd.js","_app/immutable/entry/app.CK9ESlOD.js","_app/immutable/chunks/Dc2GWTX7.js","_app/immutable/chunks/BOYLDhuR.js","_app/immutable/chunks/CY-7o9oA.js","_app/immutable/chunks/mBLmYUx9.js","_app/immutable/chunks/BNwFUchd.js"],stylesheets:[],fonts:[],uses_env_dynamic_public:false},
		nodes: [
			__memo(() => import('./chunks/0-BVG2xq7m.js')),
			__memo(() => import('./chunks/1-DSLA4ybo.js')),
			__memo(() => import('./chunks/2-BGclgAFA.js')),
			__memo(() => import('./chunks/3-BipEMX9Z.js')),
			__memo(() => import('./chunks/4-CT4UP8iD.js')),
			__memo(() => import('./chunks/5-CHX72SIk.js')),
			__memo(() => import('./chunks/6-LW8e28t_.js')),
			__memo(() => import('./chunks/7-DFtN2wQ2.js')),
			__memo(() => import('./chunks/8-C8Zb1sFD.js')),
			__memo(() => import('./chunks/9-SzVtSz2u.js')),
			__memo(() => import('./chunks/10-DgiVYaz3.js')),
			__memo(() => import('./chunks/11-tJiSD-qn.js')),
			__memo(() => import('./chunks/12-CADZooLa.js'))
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
				id: "/app",
				pattern: /^\/app\/?$/,
				params: [],
				page: { layouts: [0,], errors: [1,], leaf: 4 },
				endpoint: null
			},
			{
				id: "/setup",
				pattern: /^\/setup\/?$/,
				params: [],
				page: { layouts: [0,2,], errors: [1,,], leaf: 5 },
				endpoint: null
			},
			{
				id: "/setup/admin",
				pattern: /^\/setup\/admin\/?$/,
				params: [],
				page: { layouts: [0,2,], errors: [1,,], leaf: 6 },
				endpoint: null
			},
			{
				id: "/setup/clients",
				pattern: /^\/setup\/clients\/?$/,
				params: [],
				page: { layouts: [0,2,], errors: [1,,], leaf: 7 },
				endpoint: null
			},
			{
				id: "/setup/finish",
				pattern: /^\/setup\/finish\/?$/,
				params: [],
				page: { layouts: [0,2,], errors: [1,,], leaf: 8 },
				endpoint: null
			},
			{
				id: "/setup/indexers",
				pattern: /^\/setup\/indexers\/?$/,
				params: [],
				page: { layouts: [0,2,], errors: [1,,], leaf: 9 },
				endpoint: null
			},
			{
				id: "/setup/metadata",
				pattern: /^\/setup\/metadata\/?$/,
				params: [],
				page: { layouts: [0,2,], errors: [1,,], leaf: 10 },
				endpoint: null
			},
			{
				id: "/setup/paths",
				pattern: /^\/setup\/paths\/?$/,
				params: [],
				page: { layouts: [0,2,], errors: [1,,], leaf: 11 },
				endpoint: null
			},
			{
				id: "/setup/policies",
				pattern: /^\/setup\/policies\/?$/,
				params: [],
				page: { layouts: [0,2,], errors: [1,,], leaf: 12 },
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
