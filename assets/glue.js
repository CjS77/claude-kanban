/* claude-kanban glue — the only hand-written JavaScript in the project.
 *
 * Owns exactly seven jobs, none of which htmx attributes can express alone:
 *   1. Stamp the board version (X-Board-Version) onto every mutating request — the optimistic-concurrency token, and,
 *      being a custom header, the CSRF guard: cross-site forms can't send it, and cross-origin fetch would need a CORS
 *      preflight the server never grants.
 *   2. Live refresh: one EventSource on /events fires the `kanban:refresh` event the #board container listens for —
 *      DEFERRED while a drag is in flight, flushed on drop, so an update never yanks the card out from under the cursor.
 *   3. Drag & drop: a SortableJS instance per ticket list, re-created after every board swap; a drop POSTs the move.
 *   4. Error toasts: htmx refuses to swap non-2xx responses by default; whitelist the codes the server retargets at #toasts.
 *   5. Client-side markdown: [data-md-src] panes fetch raw markdown once and render it locally (marked + DOMPurify).
 *   6. Modal plumbing: open the detail dialog when content lands in it; close/reset forms marked for it on success.
 *   7. Epic options sync: the create-ticket form sits in the static page shell, so its epic <select> would go stale
 *      as epics come and go — after every swap it re-mirrors the list from the OOB-refreshed filter dropdown.
 */
(() => {
    "use strict";

    // Console diagnostics — pure observability, threaded through the jobs below: SSE lifecycle, every htmx
    // request/response, board refreshes, and error toasts, all under a common prefix.
    const diag = (...args) => console.log("[kanban]", ...args);

    document.body.addEventListener("htmx:configRequest", (e) => {
        const params = e.detail.parameters;
        const entries = typeof params?.entries === "function" ? Object.fromEntries(params.entries()) : params;
        diag(`→ ${e.detail.verb.toUpperCase()} ${e.detail.path}`, entries && Object.keys(entries).length ? entries : "");
    });
    document.body.addEventListener("htmx:afterRequest", (e) => {
        const cfg = e.detail.requestConfig;
        diag(`← ${e.detail.xhr.status} ${cfg.verb.toUpperCase()} ${cfg.path}${e.detail.successful ? "" : " (failed)"}`);
    });
    document.body.addEventListener("htmx:afterSwap", (e) => {
        diag(`swapped #${e.detail.target.id || e.detail.target.tagName}`);
    });

    // --- 1. board version header ---------------------------------------------------------------------------------
    document.body.addEventListener("htmx:configRequest", (e) => {
        if (e.detail.verb !== "get") {
            const root = document.getElementById("board-root");
            e.detail.headers["X-Board-Version"] = (root && root.dataset.version) || "0";
        }
    });

    // --- 2. live refresh, drag-guarded ----------------------------------------------------------------------------
    let dragging = false;
    let pendingRefresh = false;

    const refresh = () => {
        if (dragging) {
            diag("board refresh deferred — drag in flight");
            pendingRefresh = true;
            return;
        }
        pendingRefresh = false;
        diag("board refresh");
        htmx.trigger(document.body, "kanban:refresh");
    };

    // The server's 409 handler also asks for an immediate corrective refetch via HX-Trigger.
    document.body.addEventListener("kanban:refresh-now", refresh);

    const connect = () => {
        const es = new EventSource("/events");
        es.onopen = () => diag("SSE connected to /events");
        es.addEventListener("board-changed", (e) => {
            const root = document.getElementById("board-root");
            const alreadyShown = root && String(e.data) === root.dataset.version;
            diag(`SSE board-changed: version ${e.data}${alreadyShown ? " (already shown)" : ""}`);
            if (alreadyShown) return;
            refresh();
        });
        es.onerror = () => {
            diag("SSE connection lost — retrying in 2s");
            es.close();
            setTimeout(connect, 2000); // server restarted or M4 not running yet — keep trying quietly
        };
    };
    connect();

    // --- 3. drag & drop --------------------------------------------------------------------------------------------
    const initDragAndDrop = (scope) => {
        const root = document.getElementById("board-root");
        // A filtered board hides cards, so a drop index among visible cards would be meaningless — no dragging.
        if (!root || root.dataset.draggable !== "true") return;
        scope.querySelectorAll(".ticket-list").forEach((list) => {
            if (list._sortable) return;
            list._sortable = Sortable.create(list, {
                group: "board",
                animation: 150,
                ghostClass: "opacity-40",
                onStart: () => {
                    dragging = true;
                },
                onEnd: (evt) => {
                    dragging = false;
                    const id = evt.item.dataset.id;
                    const to = evt.to.dataset.column;
                    if (id && to) {
                        htmx.ajax("POST", `/ui/ticket/${id}/move`, { values: { to, position: evt.newIndex }, swap: "none" });
                    }
                    if (pendingRefresh) refresh(); // a change arrived mid-drag; apply it now
                },
            });
        });
    };

    // --- 4. error toasts -------------------------------------------------------------------------------------------
    document.body.addEventListener("htmx:beforeSwap", (e) => {
        if ([400, 403, 404, 409, 422].includes(e.detail.xhr.status)) {
            e.detail.shouldSwap = true; // server retargeted the response at #toasts
            e.detail.isError = false;
        }
    });

    const toasts = document.getElementById("toasts");
    if (toasts) {
        new MutationObserver((mutations) => {
            mutations.forEach((m) =>
                m.addedNodes.forEach((node) => {
                    if (node.nodeType !== 1) return;
                    console.warn("[kanban] toast:", node.textContent.trim());
                    setTimeout(() => node.remove(), 6000);
                })
            );
        }).observe(toasts, { childList: true });
    }

    // --- 5. client-side markdown ------------------------------------------------------------------------------------
    const renderMarkdown = (scope) => {
        // htmx:load fires once per TOP-LEVEL element of a swapped-in fragment, so a pane can BE the scope itself
        // (detail.html's <article> is one) — querySelectorAll alone only sees descendants and would skip it.
        const panes = [...scope.querySelectorAll("[data-md-src]")];
        if (scope.matches && scope.matches("[data-md-src]")) panes.unshift(scope);
        panes.forEach((el) => {
            const src = el.dataset.mdSrc;
            el.removeAttribute("data-md-src");
            fetch(src)
                .then((res) => (res.ok ? res.text() : Promise.reject(res.status)))
                .then((md) => {
                    el.innerHTML = DOMPurify.sanitize(marked.parse(md));
                })
                .catch(() => {
                    el.textContent = "failed to load body";
                });
        });
    };

    // --- 6. modal plumbing ------------------------------------------------------------------------------------------
    document.body.addEventListener("htmx:afterSwap", (e) => {
        if (e.detail.target.id === "detail" && e.detail.target.innerHTML.trim() !== "") {
            const modal = document.getElementById("detail-modal");
            if (modal && !modal.open) modal.showModal();
        }
    });

    document.body.addEventListener("htmx:afterRequest", (e) => {
        if (!e.detail.successful) return;
        const el = e.detail.elt;
        if (el.hasAttribute && el.hasAttribute("data-reset-on-success") && el.tagName === "FORM") el.reset();
        if (el.hasAttribute && el.hasAttribute("data-close-modal")) {
            const dialog = el.closest("dialog");
            if (dialog) dialog.close();
        }
    });

    // --- 7. epic options sync ---------------------------------------------------------------------------------------
    // #filter-epic is swapped out-of-band with every board fragment, so it always holds the current epic list; the
    // create-ticket form's <select> is copied from it rather than OOB-swapped itself, which would wipe the user's
    // in-flight choice on every live refresh. Each select keeps its own first option ("none" / "All epics").
    const syncEpicOptions = () => {
        const source = document.getElementById("filter-epic");
        if (!source) return;
        document.querySelectorAll("select[data-epic-options]").forEach((select) => {
            const current = select.value;
            [...select.options].slice(1).forEach((option) => option.remove());
            [...source.options].filter((option) => option.value !== "").forEach((option) => select.add(new Option(option.text, option.value)));
            select.value = [...select.options].some((option) => option.value === current) ? current : "";
        });
    };

    // htmx calls this once per swapped-in element (and once for body on load): wire up whatever arrived.
    htmx.onLoad((el) => {
        const scope = el.nodeType === 1 ? el : document.body;
        initDragAndDrop(scope);
        renderMarkdown(scope);
        syncEpicOptions();
    });
})();
