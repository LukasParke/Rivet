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
		client: {start:"_app/immutable/entry/start.ylfvT_hj.js",app:"_app/immutable/entry/app.1lpqzDLH.js",imports:["_app/immutable/entry/start.ylfvT_hj.js","_app/immutable/chunks/BsV3MsZ9.js","_app/immutable/chunks/Bhh8yegK.js","_app/immutable/chunks/Cv35kFbH.js","_app/immutable/entry/app.1lpqzDLH.js","_app/immutable/chunks/Bhh8yegK.js","_app/immutable/chunks/BtkahRsy.js","_app/immutable/chunks/DgmcA2ey.js","_app/immutable/chunks/OMZJLTk2.js","_app/immutable/chunks/BNTHtUbE.js","_app/immutable/chunks/Cv35kFbH.js"],stylesheets:[],fonts:[],uses_env_dynamic_public:false},
		nodes: [
			__memo(() => import('./chunks/0-BpwXXaU4.js')),
			__memo(() => import('./chunks/1-DKWNdhuW.js')),
			__memo(() => import('./chunks/2-8gnKYzcS.js')),
			__memo(() => import('./chunks/3-ZXikdEqg.js')),
			__memo(() => import('./chunks/4-ojlNLKui.js')),
			__memo(() => import('./chunks/5-Dsiz8Ooc.js')),
			__memo(() => import('./chunks/6-D0yxvaiC.js')),
			__memo(() => import('./chunks/7-Dj5rUsk0.js')),
			__memo(() => import('./chunks/8-Dmdw_03b.js')),
			__memo(() => import('./chunks/9-Bwk_LryV.js')),
			__memo(() => import('./chunks/10-DXDSMbrf.js')),
			__memo(() => import('./chunks/11-BqAiCD7Z.js')),
			__memo(() => import('./chunks/12-BG2nrn62.js')),
			__memo(() => import('./chunks/13-ChdfF9Vb.js')),
			__memo(() => import('./chunks/14-DOvBrKRF.js'))
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
				id: "/app/review",
				pattern: /^\/app\/review\/?$/,
				params: [],
				page: { layouts: [0,], errors: [1,], leaf: 5 },
				endpoint: null
			},
			{
				id: "/login",
				pattern: /^\/login\/?$/,
				params: [],
				page: { layouts: [0,], errors: [1,], leaf: 6 },
				endpoint: null
			},
			{
				id: "/setup",
				pattern: /^\/setup\/?$/,
				params: [],
				page: { layouts: [0,2,], errors: [1,,], leaf: 7 },
				endpoint: null
			},
			{
				id: "/setup/admin",
				pattern: /^\/setup\/admin\/?$/,
				params: [],
				page: { layouts: [0,2,], errors: [1,,], leaf: 8 },
				endpoint: null
			},
			{
				id: "/setup/clients",
				pattern: /^\/setup\/clients\/?$/,
				params: [],
				page: { layouts: [0,2,], errors: [1,,], leaf: 9 },
				endpoint: null
			},
			{
				id: "/setup/finish",
				pattern: /^\/setup\/finish\/?$/,
				params: [],
				page: { layouts: [0,2,], errors: [1,,], leaf: 10 },
				endpoint: null
			},
			{
				id: "/setup/indexers",
				pattern: /^\/setup\/indexers\/?$/,
				params: [],
				page: { layouts: [0,2,], errors: [1,,], leaf: 11 },
				endpoint: null
			},
			{
				id: "/setup/metadata",
				pattern: /^\/setup\/metadata\/?$/,
				params: [],
				page: { layouts: [0,2,], errors: [1,,], leaf: 12 },
				endpoint: null
			},
			{
				id: "/setup/paths",
				pattern: /^\/setup\/paths\/?$/,
				params: [],
				page: { layouts: [0,2,], errors: [1,,], leaf: 13 },
				endpoint: null
			},
			{
				id: "/setup/policies",
				pattern: /^\/setup\/policies\/?$/,
				params: [],
				page: { layouts: [0,2,], errors: [1,,], leaf: 14 },
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
