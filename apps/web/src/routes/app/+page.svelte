<script>
  import { onMount, onDestroy } from "svelte";
  import { apiFetch } from "$lib/api";
  import { connectWS } from "$lib/ws";
  import SecretInput from "$lib/components/SecretInput.svelte";

  export let data;

  let kind = "movie";
  let externalSource = "tmdb";
  let externalId = "550";
  let query = "Fight Club 1999 1080p";

  let runId = "";
  let targetId = "";
  let error = null;
  let busy = false;
  let results = null;
  let wsStatus = "disconnected";
  let events = [];
  let acquisitions = [];
  let acqError = null;
  let acqBusy = false;
  let acqStatusFilter = "";
  /** @type {Record<string, string>} per-acquisition override paths */
  let overridePaths = {};
  let wsHandle = null;
  let acqRefreshTimer = null;
  const pendingTimers = new Set();

  function later(fn, ms) {
    const id = setTimeout(() => {
      pendingTimers.delete(id);
      fn();
    }, ms);
    pendingTimers.add(id);
    return id;
  }
  let dryRun = !!data.dryRun;
  let creatingAcquisition = {};
  let acquisitionError = null;
  let eventFilter = "";
  let autoScroll = true;
  let logsContainer = null;

  // End-to-end test state
  let testRunning = false;
  let testState = "idle"; // idle, creating_target, searching, waiting_for_candidates, creating_acquisition, monitoring, verifying, importing, completed, error
  let testStartTime = null;
  let testEndTime = null;
  let testSummary = {
    targetId: null,
    runId: null,
    acquisitionId: null,
    candidateId: null,
  };
  let setupComplete = !!data.setupComplete;
  let prerequisitesError = null;

  // Configuration editor state
  let configExpanded = false;
  let configLoaded = false;
  /** Full config as returned by GET /api/v1/setup/config — source of truth
   * for sections this editor does not render, so saves never wipe them. */
  let savedConfig = null;
  let validatingAll = false;
  let validationSummary = {
    total: 0,
    passed: 0,
    failed: 0
  };
  
  // Paths configuration
  let pathsDownloadsCompleted = '/downloads/completed';
  let pathsQuarantine = '/quarantine';
  let pathsLibraryRootsRaw = '/library';
  let pathsResult = null;
  let pathsError = null;
  let pathsBusy = false;
  let pathsSaveStatus = null;

  // Clients configuration
  let clientsQbUrl = 'http://qbittorrent:8080';
  let clientsQbUser = '';
  let clientsQbPass = '';
  let clientsIncludeQb = true;
  let clientsResult = null;
  let clientsError = null;
  let clientsBusy = false;
  let clientsSaveStatus = null;

  // Indexers configuration
  let indexersTorznab = [{ name: 'Prowlarr', url: '', apiKey: '', username: '', password: '' }];
  let indexersResult = null;
  let indexersError = null;
  let indexersBusy = false;
  let indexersSaveStatus = null;
  let connectionTestResult = null;
  let connectionTestBusy = false;
  let connectionTestError = null;

  // Metadata configuration
  let metadataTmdbBaseUrl = 'https://api.themoviedb.org';
  let metadataTmdbKey = '';
  let metadataIncludeTMDB = true;
  let metadataTvdbBaseUrl = 'https://api4.thetvdb.com';
  let metadataTvdbKey = '';
  let metadataIncludeTVDB = false;
  let metadataResult = null;
  let metadataError = null;
  let metadataBusy = false;
  let metadataSaveStatus = null;

  // Policies configuration
  let policiesLanguage = 'en';
  let policiesBannedTagsRaw = 'cam\nts\nscr';
  let policiesProfileName = 'Default';
  let policiesAllowedQualitiesRaw = '1080p\n2160p';
  let policiesUpgradeAllowed = true;
  let policiesResult = null;
  let policiesError = null;
  let policiesBusy = false;
  let policiesSaveStatus = null;

  onMount(() => {
    openSocket();
    refreshAcquisitions();
  });

  onDestroy(() => {
    if (wsHandle) {
      wsHandle.close();
      wsHandle = null;
    }
    if (acqRefreshTimer !== null) {
      clearTimeout(acqRefreshTimer);
      acqRefreshTimer = null;
    }
    for (const id of pendingTimers) clearTimeout(id);
    pendingTimers.clear();
  });

  async function loadCurrentConfig() {
    try {
      const res = await apiFetch("/api/v1/setup/config");
      if (!res.ok) return;
      const data = await res.json();
      // Keep the full payload: sections without UI fields are merged back on save.
      savedConfig = data;
      
      // Load paths
      if (data.paths) {
        if (data.paths.downloadsCompletedPath) pathsDownloadsCompleted = data.paths.downloadsCompletedPath;
        if (data.paths.quarantinePath) pathsQuarantine = data.paths.quarantinePath;
        if (data.paths.libraryRoots && Array.isArray(data.paths.libraryRoots)) {
          pathsLibraryRootsRaw = data.paths.libraryRoots.join('\n');
        }
      }

      // Load clients (only qBittorrent)
      if (data.clients?.qbittorrent) {
        const qb = data.clients.qbittorrent;
        if (qb.url) clientsQbUrl = qb.url;
        if (qb.username) clientsQbUser = qb.username;
        if (qb.password) clientsQbPass = qb.password;
        clientsIncludeQb = true;
      }

      // Load indexers (only Torznab)
      if (data.indexers?.torznab && Array.isArray(data.indexers.torznab) && data.indexers.torznab.length > 0) {
        indexersTorznab = data.indexers.torznab.map((ix) => ({
          name: ix.name || 'Torznab',
          url: ix.url || '',
          apiKey: ix.apiKey || '',
          username: ix.username || '',
          password: ix.password || '',
        }));
      }

      // Load metadata
      if (data.metadata) {
        if (data.metadata.tmdb) {
          if (data.metadata.tmdb.baseUrl) metadataTmdbBaseUrl = data.metadata.tmdb.baseUrl;
          if (data.metadata.tmdb.apiKey) metadataTmdbKey = data.metadata.tmdb.apiKey;
          metadataIncludeTMDB = true;
        }
        if (data.metadata.tvdb) {
          if (data.metadata.tvdb.baseUrl) metadataTvdbBaseUrl = data.metadata.tvdb.baseUrl;
          if (data.metadata.tvdb.apiKey) metadataTvdbKey = data.metadata.tvdb.apiKey;
          metadataIncludeTVDB = true;
        }
      }

      // Load policies
      if (data.policies) {
        if (data.policies.language) policiesLanguage = data.policies.language;
        if (data.policies.bannedTags && Array.isArray(data.policies.bannedTags)) {
          policiesBannedTagsRaw = data.policies.bannedTags.join('\n');
        }
        if (data.policies.qualityProfiles && data.policies.qualityProfiles.length > 0) {
          const profile = data.policies.qualityProfiles[0];
          if (profile.name) policiesProfileName = profile.name;
          if (profile.allowedQualities && Array.isArray(profile.allowedQualities)) {
            policiesAllowedQualitiesRaw = profile.allowedQualities.join('\n');
          }
          if (typeof profile.upgradeAllowed === 'boolean') {
            policiesUpgradeAllowed = profile.upgradeAllowed;
          }
        }
      }
    } catch (e) {
      console.error('Failed to load current config:', e);
    }
  }

  async function checkPrerequisites() {
    prerequisitesError = null;
    try {
      const res = await apiFetch("/api/v1/setup/status");
      if (!res.ok) {
        prerequisitesError = "Unable to check setup status";
        return false;
      }
      const data = await res.json();
      setupComplete = !!data.setupComplete;
      if (!setupComplete) {
        prerequisitesError = "Setup wizard must be completed before running tests";
        return false;
      }
      return true;
    } catch (e) {
      prerequisitesError = "Error checking prerequisites: " + (e instanceof Error ? e.message : String(e));
      return false;
    }
  }

  function resetTest() {
    testRunning = false;
    testState = "idle";
    testStartTime = null;
    testEndTime = null;
    testSummary = {
      targetId: null,
      runId: null,
      acquisitionId: null,
      candidateId: null,
    };
    targetId = "";
    runId = "";
    results = null;
    error = null;
  }

  async function startEndToEndTest() {
    if (testRunning) return;
    
    const canStart = await checkPrerequisites();
    if (!canStart) {
      error = prerequisitesError;
      return;
    }

    // Reset state
    resetTest();
    testRunning = true;
    testState = "creating_target";
    testStartTime = new Date();
    error = null;
    events = [];

    try {
      // Step 1: Create target
      await createTarget();
      if (!targetId) {
        testState = "error";
        testRunning = false;
        testEndTime = new Date();
        return;
      }
      testSummary.targetId = targetId;

      // Step 2: Start search
      testState = "searching";
      await startSearch();
      if (!runId) {
        testState = "error";
        testRunning = false;
        testEndTime = new Date();
        return;
      }
      testSummary.runId = runId;

      // Step 3: Wait for search completion (poll and check WebSocket events)
      testState = "waiting_for_candidates";
      let searchCompleted = false;
      let attempts = 0;
      const maxAttempts = 60; // 60 seconds max wait

      while (!searchCompleted && attempts < maxAttempts) {
        await new Promise((resolve) => setTimeout(resolve, 1000));
        await refresh();
        attempts++;

        // Check if search completed via polling
        if (results?.run?.status === "completed") {
          searchCompleted = true;
          break;
        }
        if (results?.run?.status === "failed") {
          testState = "error";
          testRunning = false;
          testEndTime = new Date();
          return;
        }

        // Also check WebSocket events for completion (events array is updated by WebSocket handler)
        const searchCompleteEvent = events.find(
          (e) => e.type === "search.run.completed" && e.payload?.runId === runId
        );
        if (searchCompleteEvent) {
          searchCompleted = true;
          break;
        }
      }

      if (!searchCompleted) {
        error = "Search did not complete within timeout";
        testState = "error";
        testRunning = false;
        testEndTime = new Date();
        return;
      }

      // Step 4: Find acquisition (may be created automatically or manually)
      testState = "creating_acquisition";
      
      // First, check if acquisition was created automatically by the search job
      await new Promise((resolve) => setTimeout(resolve, 2000));
      await refreshAcquisitions();
      
      // Look for acquisition related to this target
      let foundAcquisition = acquisitions.find(
        (a) => (a.targetId === targetId || a.targetID === targetId) && 
               (a.status === "queued" || a.status === "downloading" || a.status === "completed")
      );

      // If no automatic acquisition, try to create one manually from first accepted candidate
      if (!foundAcquisition && results?.candidates?.length > 0) {
        const acceptedCandidate = results.candidates.find(
          (c) => c.decision?.Accepted || c.decision?.accepted
        );

        if (acceptedCandidate) {
          const candidateId = acceptedCandidate.candidate?.ID || acceptedCandidate.candidate?.id;
          if (candidateId) {
            testSummary.candidateId = candidateId;
            await createAcquisitionFromCandidate(candidateId);
            
            // Wait for acquisition to be created
            await new Promise((resolve) => setTimeout(resolve, 2000));
            await refreshAcquisitions();

            // Find the acquisition we just created
            foundAcquisition = acquisitions.find(
              (a) => (a.candidateId === candidateId || a.candidateID === candidateId) ||
                     (a.targetId === targetId || a.targetID === targetId)
            );
          }
        }
      }

      if (foundAcquisition) {
        testSummary.acquisitionId = foundAcquisition.id;
        if (foundAcquisition.candidateId || foundAcquisition.candidateID) {
          testSummary.candidateId = foundAcquisition.candidateId || foundAcquisition.candidateID;
        }
        // Check current status to determine next state
        if (foundAcquisition.status === "completed" || foundAcquisition.status === "downloading") {
          testState = "monitoring";
        } else if (foundAcquisition.verifyStatus === "completed") {
          testState = "importing";
        } else {
          testState = "monitoring";
        }
      } else {
        // No acquisition found - check if we have candidates
        if (results?.candidates?.length === 0) {
          error = "No candidates found in search results";
        } else {
          error = "No accepted candidates found and no acquisition was created automatically";
        }
        testState = "error";
        testRunning = false;
        testEndTime = new Date();
        return;
      }

      // Steps 5-7: Monitor acquisition through all stages
      // The WebSocket handler will update testState as events arrive
      // We'll mark as completed when import.completed event is received
      
    } catch (e) {
      error = e instanceof Error ? e.message : String(e);
      testState = "error";
      testRunning = false;
      testEndTime = new Date();
    }
  }

  async function createTarget() {
    busy = true;
    error = null;
    try {
      const res = await apiFetch("/api/v1/targets", {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ kind, externalSource, externalId, query }),
      });
      const data = await res.json().catch(() => null);
      if (!res.ok) throw new Error(data?.error || "create_failed");
      targetId = data.id;
    } catch (e) {
      error = e instanceof Error ? e.message : String(e);
    } finally {
      busy = false;
    }
  }

  async function startSearch() {
    if (!targetId) return;
    busy = true;
    error = null;
    results = null;
    try {
      const res = await apiFetch(`/api/v1/targets/${targetId}/search`, {
        method: "POST",
      });
      const data = await res.json().catch(() => null);
      if (!res.ok) throw new Error(data?.error || "start_failed");
      runId = data.runId;
    } catch (e) {
      error = e instanceof Error ? e.message : String(e);
    } finally {
      busy = false;
    }
  }

  async function refresh() {
    if (!runId) return;
    busy = true;
    error = null;
    try {
      const res = await apiFetch(`/api/v1/search/runs/${runId}`);
      const data = await res.json().catch(() => null);
      if (!res.ok) throw new Error(data?.error || "fetch_failed");
      results = data;
    } catch (e) {
      error = e instanceof Error ? e.message : String(e);
    } finally {
      busy = false;
    }
  }

  async function refreshAcquisitions() {
    acqBusy = true;
    acqError = null;
    try {
      const q = acqStatusFilter
        ? `?status=${encodeURIComponent(acqStatusFilter)}`
        : "";
      const res = await apiFetch(`/api/v1/acquisitions${q}`);
      const data = await res.json().catch(() => null);
      if (!res.ok) throw new Error(data?.error || "fetch_failed");
      acquisitions = Array.isArray(data) ? data : [];
    } catch (e) {
      acqError = e instanceof Error ? e.message : String(e);
    } finally {
      acqBusy = false;
    }
  }

  async function act(acquisitionId, action, body = null) {
    acqBusy = true;
    acqError = null;
    try {
      const res = await apiFetch(
        `/api/v1/acquisitions/${acquisitionId}/actions/${action}`,
        {
          method: "POST",
          headers: { "content-type": "application/json" },
          body: body ? JSON.stringify(body) : undefined,
        }
      );
      const data = await res.json().catch(() => null);
      if (!res.ok) throw new Error(data?.error || "action_failed");
      scheduleAcqRefresh();
    } catch (e) {
      acqError = e instanceof Error ? e.message : String(e);
    } finally {
      acqBusy = false;
    }
  }

  function scheduleAcqRefresh() {
    if (acqRefreshTimer !== null) return;
    acqRefreshTimer = later(() => {
      acqRefreshTimer = null;
      refreshAcquisitions();
    }, 500);
  }

  async function createAcquisitionFromCandidate(candidateId) {
    creatingAcquisition[candidateId] = true;
    acquisitionError = null;
    try {
      const res = await apiFetch(`/api/v1/candidates/${candidateId}/acquisition`, {
        method: "POST",
      });
      const data = await res.json().catch(() => null);
      if (!res.ok) throw new Error(data?.error || "create_failed");
      // Refresh acquisitions list to show the new one
      scheduleAcqRefresh();
      // Optionally refresh results to show updated state
      if (runId) {
        later(() => refresh(), 500);
      }
    } catch (e) {
      acquisitionError = e instanceof Error ? e.message : String(e);
    } finally {
      creatingAcquisition[candidateId] = false;
    }
  }

  function openSocket() {
    try {
      if (wsHandle) wsHandle.close();
      wsHandle = connectWS({
        channels: ["activity", "jobs", "audit", "downloads", "imports", "setup"],
        onStatus: (status) => (wsStatus = status),
        onEvent: handleWsEvent,
      });
    } catch {
      wsStatus = "error";
    }
  }

  function handleWsEvent(msg) {
    // Heartbeats drive staleness detection inside lib/ws only; keep them out
    // of the visible event feed.
    if (msg?.type === "health.snapshot") return;
    events = [
      { ts: msg.ts, type: msg.type, payload: msg.payload },
      ...events,
    ].slice(0, 200);
    if (
      typeof msg?.type === "string" &&
      (msg.type.startsWith("download.") || msg.type.startsWith("import."))
    ) {
      // Debounced: bursts of download/import events trigger a single refetch.
      scheduleAcqRefresh();
    }
    // Auto-refresh results when search completes
    if (msg?.type === "search.run.completed" && runId) {
      later(() => refresh(), 500);
    }

    // Track test state based on WebSocket events
    if (testRunning && typeof msg?.type === "string") {
      const eventType = msg.type;
      const payload = msg.payload || {};

      // Handle test.* events
      if (eventType === "test.target.creating") {
        testState = "creating_target";
      } else if (eventType === "test.target.created") {
        testState = "searching";
        if (payload.targetId) {
          testSummary.targetId = payload.targetId;
          targetId = payload.targetId;
        }
      } else if (eventType === "test.search.starting") {
        testState = "searching";
      } else if (eventType === "test.search.started") {
        testState = "waiting_for_candidates";
        if (payload.runId) {
          testSummary.runId = payload.runId;
          runId = payload.runId;
        }
      } else if (eventType === "test.acquisition.creating") {
        testState = "creating_acquisition";
      } else if (eventType === "test.acquisition.created") {
        testState = "monitoring";
        if (payload.acquisitionId) {
          testSummary.acquisitionId = payload.acquisitionId;
        }
        if (payload.candidateId) {
          testSummary.candidateId = payload.candidateId;
        }
      } else if (eventType === "test.target.error" || eventType === "test.search.error" || eventType === "test.acquisition.error") {
        testState = "error";
        testRunning = false;
        testEndTime = new Date();
      }

      // Update test state based on other events
      if (eventType === "download.enqueued" && testState === "creating_acquisition") {
        testState = "monitoring";
      } else if (eventType === "download.completed" && testState === "monitoring") {
        testState = "verifying";
      } else if (eventType === "verify.started" && testState === "verifying") {
        testState = "verifying";
      } else if (eventType === "verify.completed" && testState === "verifying") {
        testState = "importing";
      } else if (eventType === "import.started" && testState === "importing") {
        testState = "importing";
      } else if (eventType === "import.completed" && testState === "importing") {
        testState = "completed";
        testRunning = false;
        testEndTime = new Date();
      } else if (
        (eventType.includes("failed") || eventType.includes("error")) &&
        testState !== "completed" &&
        testState !== "error"
      ) {
        // Check if this error is related to our test
        const relevantId = payload.acquisitionId || payload.runId || payload.targetId;
        if (
          relevantId === testSummary.acquisitionId ||
          relevantId === testSummary.runId ||
          relevantId === testSummary.targetId
        ) {
          testState = "error";
          testRunning = false;
          testEndTime = new Date();
        }
      }
    }

    // Auto-scroll to top when new events arrive
    if (autoScroll && logsContainer) {
      logsContainer.scrollTop = 0;
    }
  }

  function copyLogs() {
    const logsText = JSON.stringify(events, null, 2);
    navigator.clipboard
      .writeText(logsText)
      .then(() => {
        // Show brief feedback
        const btn = document.getElementById("copy-logs-btn");
        if (btn) {
          const original = btn.textContent;
          btn.textContent = "Copied!";
          setTimeout(() => {
            btn.textContent = original;
          }, 2000);
        }
      })
      .catch(() => {
        // Fallback for older browsers
        const textarea = document.createElement("textarea");
        textarea.value = logsText;
        document.body.appendChild(textarea);
        textarea.select();
        document.execCommand("copy");
        document.body.removeChild(textarea);
      });
  }

  function clearLogs() {
    events = [];
  }

  function filteredEvents() {
    if (!eventFilter) return events;
    const filter = eventFilter.toLowerCase();
    return events.filter(
      (e) =>
        e.type?.toLowerCase().includes(filter) ||
        JSON.stringify(e.payload || {})
          .toLowerCase()
          .includes(filter)
    );
  }

  function getStepLabel(state) {
    const labels = {
      idle: "Ready",
      creating_target: "Creating Target",
      searching: "Running Search",
      waiting_for_candidates: "Waiting for Candidates",
      creating_acquisition: "Creating Acquisition",
      monitoring: "Monitoring Download",
      verifying: "Verifying File",
      importing: "Importing File",
      completed: "Completed",
      error: "Error",
    };
    return labels[state] || state;
  }

  function getProgressPercentage() {
    const steps = [
      "idle",
      "creating_target",
      "searching",
      "waiting_for_candidates",
      "creating_acquisition",
      "monitoring",
      "verifying",
      "importing",
      "completed",
    ];
    const currentIndex = steps.indexOf(testState);
    if (currentIndex === -1) return 0;
    return Math.round((currentIndex / (steps.length - 1)) * 100);
  }

  function getStepNumber() {
    const steps = [
      "idle",
      "creating_target",
      "searching",
      "waiting_for_candidates",
      "creating_acquisition",
      "monitoring",
      "verifying",
      "importing",
      "completed",
    ];
    const currentIndex = steps.indexOf(testState);
    return currentIndex === -1 ? 0 : currentIndex;
  }

  function getTestDuration() {
    if (!testStartTime) return null;
    const end = testEndTime || new Date();
    const duration = Math.round((end - testStartTime) / 1000);
    return duration;
  }

  // Configuration validation and save functions
  async function validatePaths() {
    pathsBusy = true;
    pathsError = null;
    pathsResult = null;
    try {
      const roots = pathsLibraryRootsRaw
        .split('\n')
        .map((s) => s.trim())
        .filter(Boolean);
      const res = await apiFetch('/api/v1/setup/paths/validate', {
        method: 'POST',
        headers: { 'content-type': 'application/json' },
        body: JSON.stringify({
          downloadsCompletedPath: pathsDownloadsCompleted,
          quarantinePath: pathsQuarantine,
          libraryRoots: roots,
        }),
      });
      const data = await res.json().catch(() => null);
      if (!res.ok) throw new Error(data?.message || data?.error || 'validate_failed');
      pathsResult = data;
    } catch (e) {
      pathsError = e instanceof Error ? e.message : String(e);
    } finally {
      pathsBusy = false;
    }
  }

  async function savePaths() {
    pathsBusy = true;
    pathsError = null;
    pathsSaveStatus = null;
    try {
      const roots = pathsLibraryRootsRaw
        .split('\n')
        .map((s) => s.trim())
        .filter(Boolean);
      const res = await apiFetch('/api/v1/setup/paths/save', {
        method: 'POST',
        headers: { 'content-type': 'application/json' },
        body: JSON.stringify({
          downloadsCompletedPath: pathsDownloadsCompleted,
          quarantinePath: pathsQuarantine,
          libraryRoots: roots,
        }),
      });
      const data = await res.json().catch(() => null);
      if (!res.ok) {
        if (data?.downloadsCompleted) {
          pathsResult = data;
          return;
        }
        throw new Error(data?.message || data?.error || 'save_failed');
      }
      pathsSaveStatus = 'saved';
      later(() => { pathsSaveStatus = null; }, 3000);
    } catch (e) {
      pathsError = e instanceof Error ? e.message : String(e);
    } finally {
      pathsBusy = false;
    }
  }

  /** Full clients map: edited qBittorrent merged over whatever /setup/config
   * returned, so NZBGet/SABnzbd entries are never wiped by a save. */
  function buildClientsPayload() {
    const clients = JSON.parse(JSON.stringify(savedConfig?.clients ?? {}));
    if (clientsIncludeQb) {
      clients.qbittorrent = { url: clientsQbUrl, username: clientsQbUser, password: clientsQbPass };
    } else {
      delete clients.qbittorrent;
    }
    return clients;
  }

  async function validateClients() {
    clientsBusy = true;
    clientsError = null;
    clientsResult = null;
    try {
      const payload = buildClientsPayload();
      const res = await apiFetch('/api/v1/setup/clients/validate', {
        method: 'POST',
        headers: { 'content-type': 'application/json' },
        body: JSON.stringify(payload),
      });
      const data = await res.json().catch(() => null);
      if (!res.ok) throw new Error(data?.message || data?.error || 'validate_failed');
      clientsResult = data;
    } catch (e) {
      clientsError = e instanceof Error ? e.message : String(e);
    } finally {
      clientsBusy = false;
    }
  }

  async function saveClients() {
    clientsBusy = true;
    clientsError = null;
    clientsSaveStatus = null;
    try {
      const payload = buildClientsPayload();
      const res = await apiFetch('/api/v1/setup/clients/save', {
        method: 'POST',
        headers: { 'content-type': 'application/json' },
        body: JSON.stringify(payload),
      });
      const data = await res.json().catch(() => null);
      if (!res.ok) {
        if (data?.qbittorrent) {
          clientsResult = data;
          return;
        }
        throw new Error(data?.message || data?.error || 'save_failed');
      }
      clientsSaveStatus = 'saved';
      later(() => { clientsSaveStatus = null; }, 3000);
    } catch (e) {
      clientsError = e instanceof Error ? e.message : String(e);
    } finally {
      clientsBusy = false;
    }
  }

  function addIndexer() {
    indexersTorznab = [...indexersTorznab, { name: 'Torznab', url: '', apiKey: '', username: '', password: '' }];
  }

  function removeIndexer(index) {
    indexersTorznab = indexersTorznab.filter((_, i) => i !== index);
  }

  async function validateIndexers() {
    indexersBusy = true;
    indexersError = null;
    indexersResult = null;
    try {
      const payload = {
        torznab: indexersTorznab.map((ix) => ({
          name: ix.name,
          url: ix.url,
          apiKey: ix.apiKey,
          username: ix.username || '',
          password: ix.password || '',
        })),
        // Preserve Newznab entries this editor does not render.
        newznab: savedConfig?.indexers?.newznab ?? [],
      };
      const res = await apiFetch('/api/v1/setup/indexers/validate', {
        method: 'POST',
        headers: { 'content-type': 'application/json' },
        body: JSON.stringify(payload),
      });
      const data = await res.json().catch(() => null);
      if (!res.ok) throw new Error(data?.message || data?.error || 'validate_failed');
      indexersResult = data;
    } catch (e) {
      indexersError = e instanceof Error ? e.message : String(e);
    } finally {
      indexersBusy = false;
    }
  }

  async function testConnection() {
    if (indexersTorznab.length === 0 || !indexersTorznab[0].url || !indexersTorznab[0].apiKey) {
      connectionTestError = "Please enter a URL and API key first";
      return;
    }
    
    connectionTestBusy = true;
    connectionTestError = null;
    connectionTestResult = null;
    try {
      const res = await apiFetch('/api/v1/diagnostics/test-connection', {
        method: 'POST',
        headers: { 'content-type': 'application/json' },
        body: JSON.stringify({
          url: indexersTorznab[0].url,
          apiKey: indexersTorznab[0].apiKey,
        }),
      });
      const data = await res.json().catch(() => null);
      if (!res.ok) throw new Error(data?.error || 'test_failed');
      connectionTestResult = data;
    } catch (e) {
      connectionTestError = e instanceof Error ? e.message : String(e);
    } finally {
      connectionTestBusy = false;
    }
  }

  async function saveIndexers() {
    indexersBusy = true;
    indexersError = null;
    indexersSaveStatus = null;
    try {
      const payload = {
        torznab: indexersTorznab.map((ix) => ({
          name: ix.name,
          url: ix.url,
          apiKey: ix.apiKey,
          username: ix.username || '',
          password: ix.password || '',
        })),
        // Preserve Newznab entries this editor does not render.
        newznab: savedConfig?.indexers?.newznab ?? [],
      };
      const res = await apiFetch('/api/v1/setup/indexers/save', {
        method: 'POST',
        headers: { 'content-type': 'application/json' },
        body: JSON.stringify(payload),
      });
      const data = await res.json().catch(() => null);
      if (!res.ok) {
        if (data?.torznab) {
          indexersResult = data;
          return;
        }
        throw new Error(data?.message || data?.error || 'save_failed');
      }
      indexersSaveStatus = 'saved';
      later(() => { indexersSaveStatus = null; }, 3000);
    } catch (e) {
      indexersError = e instanceof Error ? e.message : String(e);
    } finally {
      indexersBusy = false;
    }
  }

  /** Full metadata map: edited providers merged over whatever /setup/config
   * returned, so disabled providers are removed and unknown ones preserved. */
  function buildMetadataPayload() {
    const metadata = JSON.parse(JSON.stringify(savedConfig?.metadata ?? {}));
    if (metadataIncludeTMDB) {
      metadata.tmdb = { baseUrl: metadataTmdbBaseUrl, apiKey: metadataTmdbKey };
    } else {
      delete metadata.tmdb;
    }
    if (metadataIncludeTVDB) {
      metadata.tvdb = { baseUrl: metadataTvdbBaseUrl, apiKey: metadataTvdbKey };
    } else {
      delete metadata.tvdb;
    }
    return metadata;
  }

  async function validateMetadata() {
    metadataBusy = true;
    metadataError = null;
    metadataResult = null;
    try {
      const payload = buildMetadataPayload();
      const res = await apiFetch('/api/v1/setup/metadata/validate', {
        method: 'POST',
        headers: { 'content-type': 'application/json' },
        body: JSON.stringify(payload),
      });
      const data = await res.json().catch(() => null);
      if (!res.ok) throw new Error(data?.message || data?.error || 'validate_failed');
      metadataResult = data;
    } catch (e) {
      metadataError = e instanceof Error ? e.message : String(e);
    } finally {
      metadataBusy = false;
    }
  }

  async function saveMetadata() {
    metadataBusy = true;
    metadataError = null;
    metadataSaveStatus = null;
    try {
      const payload = buildMetadataPayload();
      const res = await apiFetch('/api/v1/setup/metadata/save', {
        method: 'POST',
        headers: { 'content-type': 'application/json' },
        body: JSON.stringify(payload),
      });
      const data = await res.json().catch(() => null);
      if (!res.ok) {
        if (data?.tmdb || data?.tvdb) {
          metadataResult = data;
          return;
        }
        throw new Error(data?.message || data?.error || 'save_failed');
      }
      metadataSaveStatus = 'saved';
      later(() => { metadataSaveStatus = null; }, 3000);
    } catch (e) {
      metadataError = e instanceof Error ? e.message : String(e);
    } finally {
      metadataBusy = false;
    }
  }

  async function validatePolicies() {
    policiesBusy = true;
    policiesError = null;
    policiesResult = null;
    try {
      const bannedTags = policiesBannedTagsRaw
        .split('\n')
        .map((s) => s.trim())
        .filter(Boolean);
      const allowedQualities = policiesAllowedQualitiesRaw
        .split('\n')
        .map((s) => s.trim())
        .filter(Boolean);
      const payload = {
        language: policiesLanguage,
        bannedTags,
        qualityProfiles: [{ name: policiesProfileName, allowedQualities, upgradeAllowed: policiesUpgradeAllowed }],
      };
      const res = await apiFetch('/api/v1/setup/policies/validate', {
        method: 'POST',
        headers: { 'content-type': 'application/json' },
        body: JSON.stringify(payload),
      });
      const data = await res.json().catch(() => null);
      policiesResult = data;
      if (!res.ok || !policiesResult?.ok) {
        throw new Error((data?.errors && data.errors.join(', ')) || data?.error || 'validate_failed');
      }
    } catch (e) {
      policiesError = e instanceof Error ? e.message : String(e);
    } finally {
      policiesBusy = false;
    }
  }

  async function validateAll() {
    if (validatingAll) return;
    
    validatingAll = true;
    validationSummary = { total: 0, passed: 0, failed: 0 };
    
    try {
      // Validate Paths
      validationSummary.total++;
      await validatePaths();
      if (pathsResult?.ok) {
        validationSummary.passed++;
      } else {
        validationSummary.failed++;
      }
      
      // Validate Clients
      validationSummary.total++;
      await validateClients();
      if (clientsResult?.ok) {
        validationSummary.passed++;
      } else {
        validationSummary.failed++;
      }
      
      // Validate Indexers
      validationSummary.total++;
      await validateIndexers();
      if (indexersResult?.ok) {
        validationSummary.passed++;
      } else {
        validationSummary.failed++;
      }
      
      // Validate Metadata
      validationSummary.total++;
      await validateMetadata();
      if (metadataResult?.ok) {
        validationSummary.passed++;
      } else {
        validationSummary.failed++;
      }
      
      // Validate Policies
      validationSummary.total++;
      await validatePolicies();
      if (policiesResult?.ok) {
        validationSummary.passed++;
      } else {
        validationSummary.failed++;
      }
    } catch (e) {
      // Error handling is done in individual validation functions
    } finally {
      validatingAll = false;
    }
  }

  async function savePolicies() {
    policiesBusy = true;
    policiesError = null;
    policiesSaveStatus = null;
    try {
      const bannedTags = policiesBannedTagsRaw
        .split('\n')
        .map((s) => s.trim())
        .filter(Boolean);
      const allowedQualities = policiesAllowedQualitiesRaw
        .split('\n')
        .map((s) => s.trim())
        .filter(Boolean);
      const payload = {
        language: policiesLanguage,
        bannedTags,
        qualityProfiles: [{ name: policiesProfileName, allowedQualities, upgradeAllowed: policiesUpgradeAllowed }],
      };
      const res = await apiFetch('/api/v1/setup/policies/save', {
        method: 'POST',
        headers: { 'content-type': 'application/json' },
        body: JSON.stringify(payload),
      });
      const data = await res.json().catch(() => null);
      if (!res.ok) {
        throw new Error((data?.errors && data.errors.join(', ')) || data?.error || 'save_failed');
      }
      policiesSaveStatus = 'saved';
      later(() => { policiesSaveStatus = null; }, 3000);
    } catch (e) {
      policiesError = e instanceof Error ? e.message : String(e);
    } finally {
      policiesBusy = false;
    }
  }

  function exportTestLogs() {
    const testLogData = {
      metadata: {
        exportedAt: new Date().toISOString(),
        dryRun: dryRun,
        setupComplete: setupComplete,
      },
      testConfiguration: {
        kind: kind,
        externalSource: externalSource,
        externalId: externalId,
        query: query,
      },
      testState: {
        running: testRunning,
        state: testState,
        stepLabel: getStepLabel(testState),
        progress: getProgressPercentage(),
        startTime: testStartTime?.toISOString() || null,
        endTime: testEndTime?.toISOString() || null,
        duration: getTestDuration(),
      },
      testSummary: {
        targetId: testSummary.targetId,
        runId: testSummary.runId,
        candidateId: testSummary.candidateId,
        acquisitionId: testSummary.acquisitionId,
      },
      errors: {
        general: error || null,
        acquisition: acquisitionError || null,
        prerequisites: prerequisitesError || null,
      },
      results: results ? {
        run: {
          id: results.run?.ID || results.run?.id || null,
          status: results.run?.status || results.run?.Status || null,
          startedAt: results.run?.startedAt || results.run?.StartedAt || null,
          completedAt: results.run?.completedAt || results.run?.CompletedAt || null,
          error: results.run?.error || results.run?.Error || null,
        },
        candidatesCount: results.candidates?.length || 0,
        acceptedCandidatesCount: results.candidates?.filter(
          (c) => c.decision?.Accepted || c.decision?.accepted
        ).length || 0,
      } : null,
      acquisitions: acquisitions.length > 0 ? acquisitions
        .filter((a) => a && (a.id || a.ID)) // Only include acquisitions with an ID
        .map((a) => ({
          id: a.id || a.ID || "unknown",
          status: a.status || a.Status || "unknown",
          verifyStatus: a.verifyStatus || a.VerifyStatus || null,
          clientKind: a.clientKind || a.ClientKind || null,
          clientName: a.clientName || a.ClientName || null,
          targetId: a.targetId || a.targetID || a.TargetID || null,
          candidateId: a.candidateId || a.candidateID || a.CandidateID || null,
          lastError: a.lastError || a.LastError || null,
        })) : [],
      events: events.map((e) => ({
        ts: e.ts,
        type: e.type,
        payload: e.payload,
      })),
    };

    // Format as both JSON and a human-readable summary
    const jsonOutput = JSON.stringify(testLogData, null, 2);
    
    const summary = `# Rivet End-to-End Test Log

## Test Configuration
- Kind: ${kind}
- Source: ${externalSource}
- External ID: ${externalId}
- Query: ${query}
- Dry Run: ${dryRun ? "Yes" : "No"}

## Test State
- Status: ${getStepLabel(testState)} (${getProgressPercentage()}%)
- Duration: ${getTestDuration() ? `${getTestDuration()}s` : "N/A"}
- Started: ${testStartTime ? new Date(testStartTime).toLocaleString() : "N/A"}
- Ended: ${testEndTime ? new Date(testEndTime).toLocaleString() : "N/A"}

## Test Summary
- Target ID: ${testSummary.targetId || "N/A"}
- Run ID: ${testSummary.runId || "N/A"}
- Candidate ID: ${testSummary.candidateId || "N/A"}
- Acquisition ID: ${testSummary.acquisitionId || "N/A"}

## Errors
${error ? `- General: ${error}` : ""}
${acquisitionError ? `- Acquisition: ${acquisitionError}` : ""}
${prerequisitesError ? `- Prerequisites: ${prerequisitesError}` : ""}
${events.filter((e) => e.type?.includes("error") || e.type?.includes("failed")).length > 0
  ? events
      .filter((e) => e.type?.includes("error") || e.type?.includes("failed"))
      .map((e) => {
        const errorMsg = e.payload?.error || e.payload?.Error || JSON.stringify(e.payload).substring(0, 100);
        return `- ${e.type}: ${errorMsg}`;
      })
      .join("\n")
  : ""}
${!error && !acquisitionError && !prerequisitesError && events.filter((e) => e.type?.includes("error") || e.type?.includes("failed")).length === 0 ? "- None" : ""}

## Results
${results ? `- Run Status: ${results.run?.status || "N/A"}
- Candidates Found: ${results.candidates?.length || 0}
- Accepted Candidates: ${results.candidates?.filter((c) => c.decision?.Accepted || c.decision?.accepted).length || 0}` : "- No results yet"}

## Acquisitions
${acquisitions.length > 0 ? acquisitions
  .filter((a) => a && (a.id || a.ID))
  .map((a) => {
    const id = a.id || a.ID || "unknown";
    const status = a.status || a.Status || "unknown";
    const verifyStatus = a.verifyStatus || a.VerifyStatus || "N/A";
    return `- ${id}: ${status} (verify: ${verifyStatus})`;
  })
  .join("\n") : "- None"}

## Events (${events.length} total)
${events.length > 0 ? events.slice(0, 20).map((e) => `- [${e.ts ? new Date(e.ts).toLocaleTimeString() : "N/A"}] ${e.type}`).join("\n") : "- No events"}
${events.length > 20 ? `\n... and ${events.length - 20} more events` : ""}

---

## Full JSON Data
\`\`\`json
${jsonOutput}
\`\`\`
`;

    return summary;
  }

  function copyTestLogs() {
    const logsText = exportTestLogs();
    navigator.clipboard
      .writeText(logsText)
      .then(() => {
        // Show brief feedback
        const btn = document.getElementById("copy-test-logs-btn");
        if (btn) {
          const original = btn.textContent;
          btn.textContent = "✅ Copied!";
          setTimeout(() => {
            btn.textContent = original;
          }, 3000);
        }
      })
      .catch(() => {
        // Fallback for older browsers
        const textarea = document.createElement("textarea");
        textarea.value = logsText;
        document.body.appendChild(textarea);
        textarea.select();
        document.execCommand("copy");
        document.body.removeChild(textarea);
        
        const btn = document.getElementById("copy-test-logs-btn");
        if (btn) {
          const original = btn.textContent;
          btn.textContent = "✅ Copied!";
          setTimeout(() => {
            btn.textContent = original;
          }, 3000);
        }
      });
  }

</script>

<div class="container">
  <h1>Rivet (MVP pipeline demo)</h1>
  <p class="muted">
    Create a target, run an indexer search, store candidates + decisions.
  </p>
  <div class="row" style="margin-bottom: 12px;">
    <a href="/app/review"><button class="secondary">Review queue</button></a>
    {#if dryRun}
      <a href="/setup"><button class="secondary">Review Setup</button></a>
    {/if}
  </div>

  <!-- Configuration Editor Section -->
  <div class="card" style="margin-bottom: 16px; background: #f8f9fa;">
    <div style="display: flex; justify-content: space-between; align-items: center; margin-bottom: 12px;">
      <h2 style="margin: 0;">⚙️ Configuration</h2>
      <div style="display: flex; gap: 8px; align-items: center;">
        {#if configExpanded && validationSummary.total > 0}
          <span style="font-size: 14px; color: {validationSummary.failed > 0 ? '#dc3545' : '#28a745'};">
            {validationSummary.failed > 0 ? '✗' : '✓'} {validationSummary.passed}/{validationSummary.total}
          </span>
        {/if}
        <button 
          disabled={validatingAll || !configExpanded} 
          on:click={validateAll}
          style="min-width: 120px;"
        >
          {validatingAll ? 'Validating...' : 'Validate All'}
        </button>
        <button class="secondary" on:click={async () => {
          configExpanded = !configExpanded;
          if (configExpanded && !configLoaded) {
            await loadCurrentConfig();
            configLoaded = true;
          }
        }}>
          {configExpanded ? "▼ Collapse" : "▶ Expand"}
        </button>
      </div>
    </div>
    
    {#if configExpanded}
      <!-- Test Configuration -->
      <details open style="margin-bottom: 16px; padding: 12px; background: white; border-radius: 4px;">
        <summary style="cursor: pointer; font-weight: bold; margin-bottom: 8px;">Test Configuration</summary>
        <div class="row">
          <input placeholder="kind (movie/episode/season_pack)" bind:value={kind} />
          <input placeholder="externalSource (tmdb/tvdb)" bind:value={externalSource} />
          <input placeholder="externalId" bind:value={externalId} />
          <input placeholder="query" bind:value={query} />
        </div>
      </details>

      <!-- Paths Configuration -->
      <details style="margin-bottom: 16px; padding: 12px; background: white; border-radius: 4px;">
        <summary style="cursor: pointer; font-weight: bold; margin-bottom: 8px;">
          Paths {pathsSaveStatus === 'saved' ? '✅' : pathsResult?.ok ? '✓' : pathsResult ? '✗' : ''}
        </summary>
        <div class="row">
          <input placeholder="Downloads completed path" bind:value={pathsDownloadsCompleted} />
          <input placeholder="Quarantine path" bind:value={pathsQuarantine} />
        </div>
        <div style="margin-top: 12px;">
          <textarea
            rows="3"
            style="width: 100%; padding: 10px 12px; border: 1px solid #cbd5e1; border-radius: 10px; font-size: 14px;"
            placeholder="Library roots (one per line)"
            bind:value={pathsLibraryRootsRaw}
          ></textarea>
        </div>
        <div class="row" style="margin-top: 12px;">
          <button disabled={pathsBusy} on:click={validatePaths}>Validate</button>
          <button class="secondary" disabled={pathsBusy || !pathsResult?.ok} on:click={savePaths}>Save</button>
        </div>
        {#if pathsError}
          <p class="error" style="margin-top: 8px;">{pathsError}</p>
        {/if}
        {#if pathsSaveStatus === 'saved'}
          <p style="color: green; margin-top: 8px;">✅ Saved successfully</p>
        {/if}
      </details>

      <!-- Clients Configuration -->
      <details style="margin-bottom: 16px; padding: 12px; background: white; border-radius: 4px;">
        <summary style="cursor: pointer; font-weight: bold; margin-bottom: 8px;">
          Download Clients {clientsSaveStatus === 'saved' ? '✅' : clientsResult?.ok ? '✓' : clientsResult ? '✗' : ''}
        </summary>
        <h3 style="margin-top: 0;">qBittorrent</h3>
        <label><input type="checkbox" bind:checked={clientsIncludeQb} /> Enable</label>
        <div class="row" style="margin-top: 8px;">
          <input placeholder="URL" bind:value={clientsQbUrl} disabled={!clientsIncludeQb} />
          <input placeholder="Username" bind:value={clientsQbUser} disabled={!clientsIncludeQb} />
          <SecretInput
            placeholder="Password"
            autocomplete="current-password"
            bind:value={clientsQbPass}
            disabled={!clientsIncludeQb}
          />
        </div>

        <div class="row" style="margin-top: 12px;">
          <button disabled={clientsBusy} on:click={validateClients}>Validate</button>
          <button class="secondary" disabled={clientsBusy || !clientsResult?.ok} on:click={saveClients}>Save</button>
        </div>
        {#if clientsError}
          <p class="error" style="margin-top: 8px;">{clientsError}</p>
        {/if}
        {#if clientsSaveStatus === 'saved'}
          <p style="color: green; margin-top: 8px;">✅ Saved successfully</p>
        {/if}
      </details>

      <!-- Indexers Configuration -->
      <details style="margin-bottom: 16px; padding: 12px; background: white; border-radius: 4px;">
        <summary style="cursor: pointer; font-weight: bold; margin-bottom: 8px;">
          Indexers {indexersSaveStatus === 'saved' ? '✅' : indexersResult?.ok ? '✓' : indexersResult ? '✗' : ''}
        </summary>
        <h3 style="margin-top: 0;">Torznab</h3>
        {#each indexersTorznab as indexer, i (i)}
          <div style="margin-bottom: 12px; padding: 8px; background: #f5f5f5; border-radius: 4px;">
            <div class="row" style="margin-bottom: 8px;">
              <input placeholder="Name" bind:value={indexer.name} style="flex: 1;" />
              <button class="secondary" disabled={indexersTorznab.length <= 1} on:click={() => removeIndexer(i)}>Remove</button>
            </div>
            <div class="row">
              <input placeholder="Base URL" bind:value={indexer.url} />
              <SecretInput placeholder="API key" bind:value={indexer.apiKey} />
              <input placeholder="Username (optional)" bind:value={indexer.username} />
              <SecretInput placeholder="Password (optional)" bind:value={indexer.password} />
            </div>
          </div>
        {/each}
        <button class="secondary" on:click={addIndexer} style="margin-bottom: 16px;">+ Add Torznab</button>

        <div class="row" style="margin-top: 12px;">
          <button disabled={indexersBusy} on:click={validateIndexers}>Validate</button>
          <button disabled={connectionTestBusy || indexersTorznab.length === 0 || !indexersTorznab[0].url || !indexersTorznab[0].apiKey} on:click={testConnection}>
            {connectionTestBusy ? 'Testing...' : 'Test Connection'}
          </button>
          <button class="secondary" disabled={indexersBusy || !indexersResult?.ok} on:click={saveIndexers}>Save</button>
        </div>
        {#if indexersError}
          <p class="error" style="margin-top: 8px;">{indexersError}</p>
        {/if}
        {#if connectionTestError}
          <p class="error" style="margin-top: 8px;">Connection Test Error: {connectionTestError}</p>
        {/if}
        {#if connectionTestResult}
          <div style="margin-top: 12px; padding: 12px; background: #f8f9fa; border-radius: 4px; font-size: 12px;">
            <h4 style="margin: 0 0 8px 0;">Connection Test Results</h4>
            <p style="margin: 4px 0;"><strong>Summary:</strong> {connectionTestResult.summary}</p>
            <p style="margin: 4px 0;"><strong>DNS Resolution:</strong> {connectionTestResult.dnsResolution.ips?.join(', ') || connectionTestResult.dnsResolution.error || 'N/A'}</p>
            <p style="margin: 4px 0;"><strong>TCP Test:</strong> {connectionTestResult.tcpTest.success ? '✓ Success' : '✗ Failed'} {connectionTestResult.tcpTest.error ? `(${connectionTestResult.tcpTest.error})` : ''}</p>
            <p style="margin: 4px 0;"><strong>HTTP (Default):</strong> {connectionTestResult.httpTestDefault.success ? '✓ Success' : '✗ Failed'} {connectionTestResult.httpTestDefault.error ? `(${connectionTestResult.httpTestDefault.error})` : ''} {connectionTestResult.httpTestDefault.statusCode ? `[${connectionTestResult.httpTestDefault.statusCode}]` : ''}</p>
            <p style="margin: 4px 0;"><strong>HTTP (Custom):</strong> {connectionTestResult.httpTestCustom.success ? '✓ Success' : '✗ Failed'} {connectionTestResult.httpTestCustom.error ? `(${connectionTestResult.httpTestCustom.error})` : ''} {connectionTestResult.httpTestCustom.statusCode ? `[${connectionTestResult.httpTestCustom.statusCode}]` : ''}</p>
            {#if connectionTestResult.networkInterfaces && connectionTestResult.networkInterfaces.length > 0}
              <details style="margin-top: 8px;">
                <summary style="cursor: pointer;">Network Interfaces ({connectionTestResult.networkInterfaces.length})</summary>
                {#each connectionTestResult.networkInterfaces as iface}
                  <p style="margin: 2px 0; padding-left: 16px;"><strong>{iface.name}:</strong> {iface.ips.join(', ')}</p>
                {/each}
              </details>
            {/if}
          </div>
        {/if}
        {#if indexersSaveStatus === 'saved'}
          <p style="color: green; margin-top: 8px;">✅ Saved successfully</p>
        {/if}
      </details>

      <!-- Metadata Configuration -->
      <details style="margin-bottom: 16px; padding: 12px; background: white; border-radius: 4px;">
        <summary style="cursor: pointer; font-weight: bold; margin-bottom: 8px;">
          Metadata Providers {metadataSaveStatus === 'saved' ? '✅' : metadataResult?.ok ? '✓' : metadataResult ? '✗' : ''}
        </summary>
        <h3 style="margin-top: 0;">TMDB</h3>
        <label><input type="checkbox" bind:checked={metadataIncludeTMDB} /> Enable</label>
        <div class="row" style="margin-top: 8px;">
          <input placeholder="Base URL" bind:value={metadataTmdbBaseUrl} disabled={!metadataIncludeTMDB} />
          <SecretInput
            placeholder="API key (optional)"
            bind:value={metadataTmdbKey}
            disabled={!metadataIncludeTMDB}
          />
        </div>

        <h3 style="margin-top: 16px;">TVDB</h3>
        <label><input type="checkbox" bind:checked={metadataIncludeTVDB} /> Enable</label>
        <div class="row" style="margin-top: 8px;">
          <input placeholder="Base URL" bind:value={metadataTvdbBaseUrl} disabled={!metadataIncludeTVDB} />
          <SecretInput
            placeholder="API key"
            bind:value={metadataTvdbKey}
            disabled={!metadataIncludeTVDB}
          />
        </div>

        <div class="row" style="margin-top: 12px;">
          <button disabled={metadataBusy} on:click={validateMetadata}>Validate</button>
          <button class="secondary" disabled={metadataBusy || !metadataResult?.ok} on:click={saveMetadata}>Save</button>
        </div>
        {#if metadataError}
          <p class="error" style="margin-top: 8px;">{metadataError}</p>
        {/if}
        {#if metadataSaveStatus === 'saved'}
          <p style="color: green; margin-top: 8px;">✅ Saved successfully</p>
        {/if}
      </details>

      <!-- Policies Configuration -->
      <details style="margin-bottom: 16px; padding: 12px; background: white; border-radius: 4px;">
        <summary style="cursor: pointer; font-weight: bold; margin-bottom: 8px;">
          Policies {policiesSaveStatus === 'saved' ? '✅' : policiesResult?.ok ? '✓' : policiesResult ? '✗' : ''}
        </summary>
        <div class="row">
          <input placeholder="Language (e.g. en)" bind:value={policiesLanguage} />
          <input placeholder="Profile name" bind:value={policiesProfileName} />
        </div>
        <div style="margin-top: 12px;">
          <label class="muted" for="allowed_qualities">Allowed qualities (one per line)</label>
          <textarea
            id="allowed_qualities"
            rows="3"
            style="width: 100%; padding: 10px 12px; border: 1px solid #cbd5e1; border-radius: 10px; font-size: 14px;"
            bind:value={policiesAllowedQualitiesRaw}
          ></textarea>
        </div>
        <div style="margin-top: 12px;">
          <label class="muted" for="banned_tags">Banned tags (one per line)</label>
          <textarea
            id="banned_tags"
            rows="3"
            style="width: 100%; padding: 10px 12px; border: 1px solid #cbd5e1; border-radius: 10px; font-size: 14px;"
            bind:value={policiesBannedTagsRaw}
          ></textarea>
        </div>
        <div style="margin-top: 12px;">
          <label><input type="checkbox" bind:checked={policiesUpgradeAllowed} /> Allow upgrades</label>
        </div>
        <div class="row" style="margin-top: 12px;">
          <button disabled={policiesBusy} on:click={validatePolicies}>Validate</button>
          <button class="secondary" disabled={policiesBusy} on:click={savePolicies}>Save</button>
        </div>
        {#if policiesError}
          <p class="error" style="margin-top: 8px;">{policiesError}</p>
        {/if}
        {#if policiesSaveStatus === 'saved'}
          <p style="color: green; margin-top: 8px;">✅ Saved successfully</p>
        {/if}
      </details>
    {/if}
  </div>

  <!-- End-to-End Test Section -->
  <div class="card" style="margin-bottom: 16px; background: #f8f9fa;">
    <h2 style="margin-top: 0;">End-to-End Test</h2>
    <p class="muted" style="margin-bottom: 12px;">
      Run the complete pipeline automatically: Target → Search → Acquisition → Monitor → Verify → Import
    </p>
    
    {#if prerequisitesError}
      <div style="padding: 12px; background: #fff3cd; border-radius: 4px; margin-bottom: 12px;">
        <p class="error" style="margin: 0;">⚠️ {prerequisitesError}</p>
      </div>
    {/if}

    {#if testRunning || testState !== "idle"}
      <div style="margin-bottom: 16px;">
        <div style="display: flex; justify-content: space-between; align-items: center; margin-bottom: 8px;">
          <div>
            <strong>Status: {getStepLabel(testState)}</strong>
            {#if testStartTime}
              <span class="muted" style="margin-left: 8px;">
                ({getTestDuration()}s)
              </span>
            {/if}
          </div>
          {#if testState === "completed"}
            <span style="color: green; font-weight: bold;">✅ Test Completed</span>
          {:else if testState === "error"}
            <span style="color: red; font-weight: bold;">❌ Test Failed</span>
          {/if}
        </div>
        <div style="width: 100%; height: 24px; background: #e0e0e0; border-radius: 12px; overflow: hidden;">
          <div
            style="height: 100%; background: {testState === 'error'
              ? '#f44336'
              : testState === 'completed'
                ? '#4caf50'
                : '#4a9eff'}; width: {getProgressPercentage()}%; transition: width 0.3s ease; display: flex; align-items: center; justify-content: center; color: white; font-size: 0.85em; font-weight: bold;"
          >
            {getProgressPercentage()}%
          </div>
        </div>
        <div style="margin-top: 8px; font-size: 0.9em; color: #666;">
          Step {getStepNumber()} of 8: {getStepLabel(testState)}
        </div>
      </div>

      {#if testSummary.targetId || testSummary.runId || testSummary.acquisitionId}
        <details style="margin-bottom: 12px;">
          <summary class="muted" style="cursor: pointer;">Test Summary</summary>
          <div style="margin-top: 8px; padding: 8px; background: white; border-radius: 4px; font-size: 0.9em;">
            {#if testSummary.targetId}
              <div>Target ID: <code>{testSummary.targetId}</code></div>
            {/if}
            {#if testSummary.runId}
              <div>Run ID: <code>{testSummary.runId}</code></div>
            {/if}
            {#if testSummary.candidateId}
              <div>Candidate ID: <code>{testSummary.candidateId}</code></div>
            {/if}
            {#if testSummary.acquisitionId}
              <div>Acquisition ID: <code>{testSummary.acquisitionId}</code></div>
            {/if}
          </div>
        </details>
      {/if}
    {/if}

    <div class="row" style="gap: 8px;">
      <button
        disabled={testRunning || !setupComplete}
        on:click={startEndToEndTest}
        style="flex: 1; font-size: 1.1em; padding: 12px;"
      >
        {testRunning ? "Test Running..." : "🚀 Start End-to-End Test"}
      </button>
      {#if testState !== "idle"}
        <button class="secondary" on:click={resetTest}>Reset Test</button>
        <button
          id="copy-test-logs-btn"
          class="secondary"
          on:click={copyTestLogs}
          title="Copy test logs for sharing/debugging"
        >
          📋 Export Test Logs
        </button>
      {/if}
    </div>
  </div>


  <div class="card" style="margin-top: 16px;">
    <div
      style="display: flex; justify-content: space-between; align-items: center; margin-bottom: 12px;"
    >
      <h2 style="margin: 0;">Realtime feed (WebSocket)</h2>
      <div class="row" style="gap: 8px; align-items: center;">
        <span class="muted" style="font-size: 0.9em;">
          {events.length} event{events.length !== 1 ? "s" : ""}
          {eventFilter ? ` (${filteredEvents().length} filtered)` : ""}
        </span>
        <label
          style="display: flex; align-items: center; gap: 4px; font-size: 0.9em; cursor: pointer;"
        >
          <input type="checkbox" bind:checked={autoScroll} />
          <span class="muted">Auto-scroll</span>
        </label>
      </div>
    </div>
    <p class="muted" style="margin-bottom: 12px;">
      Status:
      <code
        style="padding: 2px 6px; border-radius: 3px; background: {wsStatus ===
        'connected'
          ? '#d4edda'
          : wsStatus === 'error'
            ? '#f8d7da'
            : '#fff3cd'};">{wsStatus}</code
      >
    </p>
    <div class="row" style="gap: 8px; margin-bottom: 12px; flex-wrap: wrap;">
      <button class="secondary" disabled={busy} on:click={openSocket}>
        Reconnect
      </button>
      <button
        id="copy-logs-btn"
        class="secondary"
        disabled={events.length === 0}
        on:click={copyLogs}
      >
        📋 Copy Logs
      </button>
      <button
        class="secondary"
        disabled={events.length === 0}
        on:click={clearLogs}
      >
        🗑️ Clear
      </button>
      <input
        type="text"
        placeholder="Filter events (type or content)..."
        bind:value={eventFilter}
        style="flex: 1; min-width: 200px;"
      />
    </div>
    <div
      bind:this={logsContainer}
      style="max-height: 400px; overflow: auto; background: #1e1e1e; border-radius: 4px; padding: 12px; font-family: 'Monaco', 'Menlo', 'Ubuntu Mono', monospace; font-size: 0.85em;"
    >
      {#if filteredEvents().length === 0}
        <p class="muted" style="margin: 0; color: #888;">
          {eventFilter
            ? "No events match the filter."
            : "No events yet. Waiting for WebSocket messages..."}
        </p>
      {:else}
        {#each filteredEvents() as event (event.ts + event.type)}
          {@const stepLabel = testRunning && testSummary.runId && event.payload?.runId === testSummary.runId
            ? "🔍 Search"
            : testRunning && testSummary.acquisitionId && (event.payload?.acquisitionId === testSummary.acquisitionId || event.type?.startsWith("download.") || event.type?.startsWith("verify.") || event.type?.startsWith("import."))
              ? event.type?.startsWith("download.")
                ? "⬇️ Download"
                : event.type?.startsWith("verify.")
                  ? "✓ Verify"
                  : event.type?.startsWith("import.")
                    ? "📦 Import"
                    : "🔄 Pipeline"
              : ""}
          <div
            style="margin-bottom: 12px; padding: 8px; background: #2d2d2d; border-radius: 3px; border-left: 3px solid {event.type?.startsWith(
              'search.'
            )
              ? '#4a9eff'
              : event.type?.startsWith('job.')
                ? '#4caf50'
                : event.type?.startsWith('download.')
                  ? '#9c27b0'
                  : event.type?.startsWith('verify.')
                    ? '#ff9800'
                    : event.type?.startsWith('import.')
                      ? '#00bcd4'
                      : event.type?.startsWith('error') ||
                          event.type?.includes('error') ||
                          event.type?.includes('failed')
                        ? '#f44336'
                        : '#888'};"
          >
            <div
              style="display: flex; justify-content: space-between; align-items: start; margin-bottom: 4px;"
            >
              <div>
                {#if stepLabel}
                  <span style="color: #888; font-size: 0.8em; margin-right: 8px;">{stepLabel}</span>
                {/if}
                <span style="color: #4a9eff; font-weight: bold;"
                  >{event.type || "unknown"}</span
                >
                <span
                  class="muted"
                  style="color: #888; font-size: 0.9em; margin-left: 8px;"
                >
                  {event.ts ? new Date(event.ts).toLocaleTimeString() : ""}
                </span>
              </div>
            </div>
            <details style="margin-top: 4px;">
              <summary style="color: #888; cursor: pointer; font-size: 0.9em;"
                >View payload</summary
              >
              <pre
                style="margin: 8px 0 0 0; color: #d4d4d4; overflow-x: auto;">{JSON.stringify(
                  event.payload || {},
                  null,
                  2
                )}</pre>
            </details>
          </div>
        {/each}
      {/if}
    </div>
  </div>

  <div class="card" style="margin-top: 16px;">
    <h2>Acquisitions</h2>
    <p class="muted">
      Live updates from download/import events. Filter by status to use this as
      a minimal “review queue”.
    </p>
    <div class="row">
      <input
        placeholder="status filter (e.g. needs_review)"
        bind:value={acqStatusFilter}
      />
      <button
        class="secondary"
        disabled={acqBusy}
        on:click={refreshAcquisitions}>Refresh acquisitions</button
      >
    </div>
    {#if acqError}
      <p class="error">{acqError}</p>
    {/if}
    {#if acquisitions?.length}
      <div style="display: grid; gap: 12px; margin-top: 12px;">
        {#each acquisitions as a (a.id)}
          <div class="card" style="margin: 0;">
            <div
              class="row"
              style="justify-content: space-between; align-items: center;"
            >
              <div>
                <div><strong>{a.id}</strong></div>
                <div class="muted">
                  status: <code>{a.status}</code> client:
                  <code>{a.clientKind}</code>
                  verify: <code>{a.verifyStatus ?? ""}</code>
                </div>
                {#if a.lastError}<div class="error">
                    lastError: {a.lastError}
                  </div>{/if}
              </div>
              <div class="row" style="gap: 8px; flex-wrap: wrap;">
                <button
                  class="secondary"
                  disabled={acqBusy}
                  on:click={() => act(a.id, "pause")}>Pause</button
                >
                <button
                  class="secondary"
                  disabled={acqBusy}
                  on:click={() => act(a.id, "resume")}>Resume</button
                >
                <button
                  class="secondary"
                  disabled={acqBusy}
                  on:click={() => act(a.id, "retry")}>Retry</button
                >
                {#if a.status === "needs_review"}
                  <button
                    class="secondary"
                    disabled={acqBusy}
                    on:click={() => act(a.id, "reverify")}>Re-verify</button
                  >
                  <button
                    class="secondary"
                    disabled={acqBusy}
                    on:click={() => act(a.id, "reimport")}>Re-import</button
                  >
                {/if}
              </div>
            </div>
            <details style="margin-top: 8px;">
              <summary class="muted">Details</summary>
              <pre style="max-height: 240px; overflow: auto;">{JSON.stringify(
                  a,
                  null,
                  2
                )}</pre>
            </details>
            {#if a.status === "needs_review"}
              <div class="row" style="margin-top: 8px;">
                <input
                  placeholder="override importedPath (admin)"
                  aria-label={`Override importedPath for ${a.id}`}
                  value={overridePaths[a.id] ?? ''}
                  on:input={(e) => (overridePaths[a.id] = e.currentTarget.value)}
                />
                <button
                  class="secondary"
                  disabled={acqBusy || !(overridePaths[a.id] ?? '').trim()}
                  on:click={() =>
                    act(a.id, "override_imported", {
                      importedPath: (overridePaths[a.id] ?? '').trim(),
                    })}
                >
                  Mark imported
                </button>
              </div>
            {/if}
          </div>
        {/each}
      </div>
    {:else}
      <p class="muted">No acquisitions yet.</p>
    {/if}
  </div>

  {#if results}
    <div class="card" style="margin-top: 16px;">
      <h2>Search Results</h2>
      <p class="muted" style="margin-bottom: 12px;">
        Run Status: <code>{results.run.status}</code>
        {#if results.run.startedAt}
          | Started: <code
            >{new Date(results.run.startedAt).toLocaleString()}</code
          >
        {/if}
        {#if results.run.completedAt}
          | Completed: <code
            >{new Date(results.run.completedAt).toLocaleString()}</code
          >
        {/if}
        {#if results.run.error}
          | <span class="error">Error: {results.run.error}</span>
        {/if}
      </p>
      {#if acquisitionError}
        <p class="error" style="margin-bottom: 12px;">{acquisitionError}</p>
      {/if}
      <details style="margin-bottom: 12px;">
        <summary class="muted" style="cursor: pointer;"
          >ℹ️ How to use search results</summary
        >
        <div
          style="margin-top: 8px; padding: 8px; background: #f5f5f5; border-radius: 4px; font-size: 0.9em;"
        >
          <p>
            <strong>Reviewing Results:</strong> Click "Refresh run" after a search
            completes to see all candidates and their decisions.
          </p>
          <p>
            <strong>Automatic Acquisitions:</strong> When a decision is accepted
            (score meets policy), an acquisition is automatically created and queued
            for download.
          </p>
          <p>
            <strong>Manual Trigger:</strong> Click "Create Acquisition" on any candidate
            to manually trigger an acquisition, even if it was rejected.
          </p>
          <p>
            <strong>View Acquisitions:</strong> Check the "Acquisitions" section
            below or the "Review queue" page to see download progress and manage
            acquisitions.
          </p>
        </div>
      </details>
      {#if results.candidates?.length}
        <div style="display: grid; gap: 12px; margin-top: 12px;">
          {#each results.candidates as candidate (candidate.candidate.ID || candidate.candidate.id)}
            {@const c = candidate.candidate}
            {@const raw = JSON.parse(
              c.RawJSON || c.rawJSON || c.rawJson || "{}"
            )}
            {@const decision = candidate.decision}
            <div class="card" style="margin: 0;">
              <div
                style="display: flex; justify-content: space-between; align-items: start; gap: 12px;"
              >
                <div style="flex: 1;">
                  <div style="margin-bottom: 8px;">
                    <strong>{raw.title || "Untitled"}</strong>
                  </div>
                  <div
                    class="muted"
                    style="font-size: 0.9em; margin-bottom: 4px;"
                  >
                    Indexer: <code>{c.IndexerName || c.indexerName}</code>
                    | Created:
                    <code
                      >{new Date(
                        c.CreatedAt || c.createdAt
                      ).toLocaleString()}</code
                    >
                  </div>
                  {#if decision}
                    <div style="margin-top: 8px;">
                      {#if decision.Accepted || decision.accepted}
                        <span style="color: green;">✅ Accepted</span>
                        <span class="muted"
                          >(Score: {decision.Score || decision.score})</span
                        >
                      {:else}
                        <span style="color: red;">❌ Rejected</span>
                        <span class="muted"
                          >(Score: {decision.Score || decision.score})</span
                        >
                      {/if}
                      {#if decision.ReasonsJSON || decision.reasonsJSON || decision.reasonsJson}
                        {@const reasons = JSON.parse(
                          decision.ReasonsJSON ||
                            decision.reasonsJSON ||
                            decision.reasonsJson ||
                            "[]"
                        )}
                        {#if Array.isArray(reasons) && reasons.length > 0}
                          <div style="margin-top: 4px; font-size: 0.85em;">
                            <strong>Reasons:</strong>
                            <ul style="margin: 4px 0; padding-left: 20px;">
                              {#each reasons as reason}
                                <li>{reason}</li>
                              {/each}
                            </ul>
                          </div>
                        {/if}
                      {/if}
                    </div>
                  {:else}
                    <div class="muted" style="margin-top: 8px;">
                      No decision yet
                    </div>
                  {/if}
                </div>
                <div>
                  <button
                    class="secondary"
                    disabled={creatingAcquisition[c.ID || c.id] || busy}
                    on:click={() =>
                      createAcquisitionFromCandidate(c.ID || c.id)}
                  >
                    {creatingAcquisition[c.ID || c.id]
                      ? "Creating..."
                      : "Create Acquisition"}
                  </button>
                </div>
              </div>
              <details style="margin-top: 8px;">
                <summary class="muted" style="cursor: pointer;"
                  >Raw JSON</summary
                >
                <pre
                  style="max-height: 200px; overflow: auto; margin-top: 8px;">{JSON.stringify(
                    candidate,
                    null,
                    2
                  )}</pre>
              </details>
            </div>
          {/each}
        </div>
      {:else}
        <p class="muted">No candidates found.</p>
      {/if}
    </div>
  {/if}
</div>
