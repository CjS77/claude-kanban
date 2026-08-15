/* claude-kanban glue — the only hand-written JavaScript in the project.
 *
 * Owns exactly nine jobs, none of which htmx attributes can express alone:
 *   1. Stamp the board version (X-Board-Version) onto every mutating request — the optimistic-concurrency token, and,
 *      being a custom header, the CSRF guard: cross-site forms can't send it, and cross-origin fetch would need a CORS
 *      preflight the server never grants.
 *   2. Live refresh: one EventSource on /events fires the `kanban:refresh` event the #board container listens for —
 *      DEFERRED while a drag is in flight, flushed on drop, so an update never yanks the card out from under the cursor.
 *   3. Drag & drop: a SortableJS instance per ticket list, re-created after every board swap; a drop POSTs the move.
 *   4. Error toasts: htmx refuses to swap non-2xx responses by default; whitelist the codes the server retargets at #toasts.
 *   5. Client-side markdown: [data-md-src] panes fetch raw markdown once and render it locally (marked + DOMPurify).
 *   6. Modal plumbing: open the detail (and diff) dialog when content lands in it; close/reset forms marked for it;
 *      jump the diff's TOC links inside the diff pane instead of letting the browser scroll the page.
 *   7. Epic options sync: the create-ticket form sits in the static page shell, so its epic <select> would go stale
 *      as epics come and go — after every swap it re-mirrors the list from the OOB-refreshed filter dropdown.
 *   8. Syntax highlighting: highlight.js over the diff pane's per-line code cells and the markdown panes' code blocks.
 *   9. Inline diff comments: comment on a line in the diff, and the notes drain into the review pane's comment box as
 *      `path:line — text` bullets, so the verdict the reviewer presses carries them to the agent.
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

    // --- 8. syntax highlighting (defined early: jobs 5 and 6 both call it) -----------------------------------------
    // highlight.js over any <code> carrying a `language-*` class — the diff pane's per-line cells (the server stamps the
    // file's language) and markdown fenced blocks. Code with no language is left plain, matching GitHub. Diff cells are
    // highlighted independently, so a construct spanning a hunk boundary can miscolour — acceptable, and far cheaper than
    // the diff engine that would track hljs state across lines. hljs stamps [data-highlighted], so re-swaps never re-run.
    const highlightCode = (scope) => {
        if (typeof hljs === "undefined") return;
        scope.querySelectorAll("code[class*='language-']:not([data-highlighted])").forEach((el) => hljs.highlightElement(el));
    };

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
                    highlightCode(el);
                })
                .catch(() => {
                    el.textContent = "failed to load body";
                });
        });
    };

    // --- 6. modal plumbing ------------------------------------------------------------------------------------------
    document.body.addEventListener("htmx:afterSwap", (e) => {
        const target = e.detail.target;
        if (target.id === "detail" && target.innerHTML.trim() !== "") {
            const modal = document.getElementById("detail-modal");
            if (modal && !modal.open) modal.showModal();
            drainComments(); // the review pane just arrived — hand it whatever was written in the diff (job 9)
        }
        // The diff lands in #diff-view (targeted from the detail pane's button): open its dialog over the detail modal
        // and colour the code now that it is in the DOM.
        if (target.id === "diff-view" && target.innerHTML.trim() !== "") {
            const modal = document.getElementById("diff-modal");
            if (modal && !modal.open) modal.showModal();
            highlightCode(target);
        }
        // Docs: the 📖 button drops the whole pane (TOC + first article primed) into #docs; open its dialog on arrival.
        // Subsequent TOC clicks swap only #docs-content and never re-trigger this.
        if (target.id === "docs" && target.innerHTML.trim() !== "") {
            const modal = document.getElementById("docs-modal");
            if (modal && !modal.open) modal.showModal();
        }
    });

    // The diff's TOC and body are separate scroll panes (assets/diff.css), so a plain `#f3` jump would move the wrong
    // box — the browser scrolls every scrollable ancestor, dragging the modal and the board behind it. Scroll the file
    // into view inside the diff body alone, and leave the URL hash untouched.
    document.body.addEventListener("click", (e) => {
        const link = e.target.closest?.(".diff-toc-item");
        if (!link) return;
        const file = document.getElementById(link.getAttribute("href").slice(1));
        if (!file) return;
        e.preventDefault();
        const body = file.closest(".diff-body");
        if (!body) return;
        // The summary bar is sticky at the top of the pane, so land the file below it rather than under it.
        const summary = body.querySelector(".diff-summary");
        const offset = summary ? summary.getBoundingClientRect().height : 0;
        body.scrollTop += file.getBoundingClientRect().top - body.getBoundingClientRect().top - offset;
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

    // --- 9. inline diff comments ------------------------------------------------------------------------------------
    // Click a line number in the diff, write a comment against that line, and it collects in a per-ticket buffer that
    // drains into the review pane's comment box as a `path:line — text` bullet. There is deliberately no server side:
    // the diff is stateless, the ticket note is the record, and `changes requested:` is already the agent's rework spec
    // — the line references only tell it where to look. Once drained the bullets are ordinary text in the box, so the
    // reviewer can reword or delete any of them before pressing a verdict, and the verdict posts them as it always has.
    const drafts = new Map(); // ticket id -> Map(anchor key -> {path, side, line, text})

    const draftsFor = (ticket) => drafts.get(ticket) || drafts.set(ticket, new Map()).get(ticket);
    const ticketOf = (el) => el.closest?.(".diff")?.dataset.ticket;

    // Which line a row speaks for. A deleted line has no counterpart in the new file, so it can only be quoted by its
    // old-file number — and the bullet says so, or the agent would look up the wrong line.
    const anchorOf = (row) => {
        const path = row?.closest(".diff-file")?.dataset.path;
        const side = row?.dataset.new ? "new" : "old";
        const line = row?.dataset.new || row?.dataset.old;
        return path && line ? { path, side, line, key: `${path} ${side} ${line}` } : null;
    };

    // The comment row sits immediately under its line, so a line carries at most one comment: clicking it again reopens
    // that comment for editing rather than stacking a second one underneath.
    const commentRowOf = (row) => (row.nextElementSibling?.classList.contains("diff-comment") ? row.nextElementSibling : null);
    const cells = '<td class="diff-ln"></td><td class="diff-ln"></td>';

    const showSaved = (tr, text) => {
        tr.innerHTML = `${cells}<td class="diff-code"><div class="diff-comment-box"><div class="diff-comment-text"></div>` +
            '<button type="button" class="diff-comment-del" title="delete this comment">×</button></div></td>';
        tr.querySelector(".diff-comment-text").textContent = text; // reviewer's own words — never innerHTML
    };

    const showEditor = (tr, text) => {
        tr.innerHTML = `${cells}<td class="diff-code"><div class="diff-comment-box">` +
            '<textarea class="diff-comment-input" rows="2" placeholder="Comment on this line…"></textarea>' +
            '<div class="diff-comment-actions"><button type="button" class="diff-comment-save">Save</button>' +
            '<button type="button" class="diff-comment-cancel">Cancel</button></div></div></td>';
        const input = tr.querySelector("textarea");
        input.value = text;
        input.focus();
    };

    document.body.addEventListener("click", (e) => {
        const save = e.target.closest?.(".diff-comment-save");
        const cancel = e.target.closest?.(".diff-comment-cancel");
        const del = e.target.closest?.(".diff-comment-del");
        const gutter = e.target.closest?.(".diff-ln");
        const tr = (save || cancel || del)?.closest("tr");
        const anchor = anchorOf(tr ? tr.previousElementSibling : gutter?.parentElement);
        const ticket = ticketOf(e.target);
        if (!anchor || !ticket) return;

        if (save) {
            const text = tr.querySelector("textarea").value.trim();
            if (text) {
                draftsFor(ticket).set(anchor.key, { ...anchor, text });
                showSaved(tr, text);
                diag(`inline comment on ${anchor.path}:${anchor.line}`);
            } else {
                draftsFor(ticket).delete(anchor.key); // saving it empty is how you take one back
                tr.remove();
            }
        } else if (cancel) {
            const saved = draftsFor(ticket).get(anchor.key);
            if (saved) showSaved(tr, saved.text);
            else tr.remove();
        } else if (del) {
            draftsFor(ticket).delete(anchor.key);
            tr.remove();
        } else if (gutter && /\bdl-(add|del|ctx)\b/.test(gutter.parentElement.className)) {
            // Only real code rows: the hunk headers and the comment rows' own blank gutters carry no line to anchor to.
            const row = gutter.parentElement;
            let box = commentRowOf(row);
            if (!box) {
                box = document.createElement("tr");
                box.className = "diff-comment";
                row.insertAdjacentElement("afterend", box);
            }
            showEditor(box, draftsFor(ticket).get(anchor.key)?.text || "");
        }
    });

    // Keys in the editor: ⌘/Ctrl+Enter saves, Escape backs out. Escape must not bubble — the diff sits in a <dialog>,
    // and the native handler would close the whole modal out from under someone who only meant to drop one comment.
    document.body.addEventListener("keydown", (e) => {
        const input = e.target.closest?.(".diff-comment-input");
        if (!input) return;
        const saving = (e.metaKey || e.ctrlKey) && e.key === "Enter";
        if (!saving && e.key !== "Escape") return;
        e.preventDefault();
        e.stopPropagation();
        input.closest("tr").querySelector(saving ? ".diff-comment-save" : ".diff-comment-cancel").click();
    });

    // Multi-line comments indent under their bullet so the reference line stays scannable in the note.
    const reference = (d) => `- ${d.path}:${d.line}${d.side === "old" ? " (old line)" : ""} — `;
    const bullet = (d) => `${reference(d)}${d.text.split("\n").join("\n  ")}`;

    // Drop the bullets a fresh drain supersedes: comment a line, close the diff, then comment that same line again and
    // the second remark replaces the first rather than stacking a contradictory pair under one reference. Only the
    // matching bullet and the lines indented under it go — prose, and bullets about other lines, stay put.
    const withoutSuperseded = (text, fresh) => {
        const stale = fresh.map(reference);
        let dropping = false;
        const kept = text.split("\n").filter((line) => {
            if (stale.some((ref) => line.startsWith(ref))) {
                dropping = true;
                return false;
            }
            if (dropping && line.startsWith("  ")) return false;
            dropping = false;
            return true;
        });
        return kept.join("\n").replace(/\n{3,}/g, "\n\n").trim();
    };

    // Drain into whichever review pane is open, appending below anything already typed there. Every route to a verdict
    // passes through this pane, so nothing written in the diff can reach the agent without the reviewer seeing it first.
    const drainComments = () => {
        const ticket = document.querySelector("#detail form[data-ticket]")?.dataset.ticket;
        const box = document.getElementById("review-comment");
        const pending = ticket && drafts.get(ticket);
        if (!box || !pending?.size) return;
        const fresh = [...pending.values()].sort((a, b) => a.path.localeCompare(b.path) || Number(a.line) - Number(b.line));
        box.value = [withoutSuperseded(box.value, fresh), fresh.map(bullet).join("\n")].filter(Boolean).join("\n\n");
        drafts.delete(ticket);
        diag(`drained ${fresh.length} inline comment(s) into ${ticket}'s review box`);
    };

    // Closing the diff over an already-open review pane is the other way back to the verdict buttons.
    document.getElementById("diff-modal")?.addEventListener("close", drainComments);

    // htmx calls this once per swapped-in element (and once for body on load): wire up whatever arrived.
    htmx.onLoad((el) => {
        const scope = el.nodeType === 1 ? el : document.body;
        initDragAndDrop(scope);
        renderMarkdown(scope);
        syncEpicOptions();
    });
})();
