import { a as attr } from './attributes-BeaNKpgU.js';
import { e as escape_html } from './escaping-CqgfEcN3.js';
import { p as push, c as pop } from './index-DyZnF-0d.js';

function _page($$payload, $$props) {
  push();
  let kind = "movie";
  let externalSource = "tmdb";
  let externalId = "550";
  let query = "Fight Club 1999 1080p";
  let busy = false;
  let wsStatus = "disconnected";
  let events = [];
  let wsRef = null;
  function connectWS() {
    try {
      if (wsRef) wsRef.close();
      const proto = window.location.protocol === "https:" ? "wss" : "ws";
      wsRef = new WebSocket(`${proto}://${window.location.host}/ws`);
      wsStatus = "connecting";
      wsRef.onopen = () => {
        wsStatus = "connected";
        wsRef.send(JSON.stringify({
          type: "subscribe",
          channels: ["activity", "jobs", "audit"]
        }));
      };
      wsRef.onclose = () => {
        wsStatus = "disconnected";
      };
      wsRef.onerror = () => {
        wsStatus = "error";
      };
      wsRef.onmessage = (ev) => {
        try {
          const msg = JSON.parse(ev.data);
          events = [
            {
              ts: msg.ts,
              type: msg.type,
              payload: msg.payload
            },
            ...events
          ].slice(0, 200);
        } catch {
        }
      };
    } catch {
      wsStatus = "error";
    }
  }
  if (typeof window !== "undefined") {
    setTimeout(connectWS, 0);
  }
  $$payload.out += `<div class="container"><h1>Rivet (MVP pipeline demo)</h1> <p class="muted">Create a target, run an indexer search, store candidates + decisions.</p> <div class="card"><h2>Create target</h2> <div class="row"><input placeholder="kind (movie/episode/season_pack)"${attr("value", kind)}> <input placeholder="externalSource (tmdb/tvdb)"${attr("value", externalSource)}> <input placeholder="externalId"${attr("value", externalId)}> <input placeholder="query"${attr("value", query)}> <button${attr("disabled", busy, true)}>Create</button></div> `;
  {
    $$payload.out += "<!--[!-->";
  }
  $$payload.out += `<!--]--> `;
  {
    $$payload.out += "<!--[!-->";
  }
  $$payload.out += `<!--]--> `;
  {
    $$payload.out += "<!--[!-->";
  }
  $$payload.out += `<!--]--></div> <div class="card" style="margin-top: 16px;"><h2>Realtime feed (WebSocket)</h2> <p class="muted">Status: ${escape_html(wsStatus)}</p> <div class="row"><button class="secondary"${attr("disabled", busy, true)}>Reconnect</button></div> <pre style="max-height: 340px; overflow: auto;">${escape_html(JSON.stringify(events, null, 2))}</pre></div> `;
  {
    $$payload.out += "<!--[!-->";
  }
  $$payload.out += `<!--]--></div>`;
  pop();
}

export { _page as default };
//# sourceMappingURL=_page.svelte-CqmSIdCJ.js.map
