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
		client: {start:"_app/immutable/entry/start.BYwk_NN9.js",app:"_app/immutable/entry/app.Dm1QDWDl.js",imports:["_app/immutable/entry/start.BYwk_NN9.js","_app/immutable/chunks/Dr8UVFfA.js","_app/immutable/chunks/CMLA4Spu.js","_app/immutable/chunks/D-hmC08u.js","_app/immutable/entry/app.Dm1QDWDl.js","_app/immutable/chunks/CMLA4Spu.js","_app/immutable/chunks/Bb0lR4-2.js","_app/immutable/chunks/suzeVJXb.js","_app/immutable/chunks/B6JxemTf.js","_app/immutable/chunks/D-hmC08u.js"],stylesheets:[],fonts:[],uses_env_dynamic_public:false},
		nodes: [
			__memo(() => import('./chunks/0-Dnzbko9k.js')),
			__memo(() => import('./chunks/1-B5XNB_XL.js')),
			__memo(() => import('./chunks/2-VybbhsTw.js')),
			__memo(() => import('./chunks/3-DBfePztP.js')),
			__memo(() => import('./chunks/4-MWO49mkE.js')),
			__memo(() => import('./chunks/5-DtLCKRrO.js')),
			__memo(() => import('./chunks/6-LhnbwwHN.js'))
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
				id: "/setup/paths",
				pattern: /^\/setup\/paths\/?$/,
				params: [],
				page: { layouts: [0,2,], errors: [1,,], leaf: 6 },
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
