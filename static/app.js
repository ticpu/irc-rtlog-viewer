// Page behaviour, kept out of the HTML so the Content-Security-Policy can
// forbid inline script. Each block runs only on pages carrying its element;
// URLs come from data attributes the server fills in.
(function() {
    'use strict';

    var sidebar = document.getElementById('sidebar');
    var toggle = document.getElementById('sidebar-toggle');
    if (sidebar && toggle) {
        toggle.addEventListener('click', function() {
            sidebar.classList.toggle('open');
        });
        document.querySelector('main').addEventListener('click', function() {
            sidebar.classList.remove('open');
        });
    }

    var picker = document.getElementById('date-picker');
    if (picker) {
        picker.addEventListener('change', function() {
            window.location.href = picker.value;
        });
    }

    var log = document.getElementById('log');
    var events = document.getElementById('toggle-events');
    if (log && events) {
        var applyEvents = function() {
            log.classList.toggle('hide-events', !events.checked);
        };
        events.checked = localStorage.getItem('hide-events') !== '1';
        applyEvents();
        events.addEventListener('change', function() {
            localStorage.setItem('hide-events', events.checked ? '0' : '1');
            applyEvents();
        });
    }

    if (log && log.dataset.latest) {
        var tail = new EventSource(log.dataset.latest);
        var atBottom = true;
        window.addEventListener('scroll', function() {
            atBottom = (window.innerHeight + window.scrollY) >= (document.body.offsetHeight - 50);
        });
        tail.onmessage = function(e) {
            log.insertAdjacentHTML('beforeend', e.data);
            if (atBottom) window.scrollTo(0, document.body.scrollHeight);
        };
    }

    var form = document.getElementById('ask-form');
    if (form) {
        var askLog = document.getElementById('ask-log');
        var links = document.getElementById('ask-links');
        var result = document.getElementById('ask-result');
        var btn = form.querySelector('button');
        var input = form.querySelector('input');
        var setBusy = function(busy) {
            btn.disabled = busy;
            input.disabled = busy;
            btn.textContent = busy ? 'working…' : 'ask AI';
        };
        form.addEventListener('submit', function(e) {
            e.preventDefault();
            var q = input.value.trim();
            if (!q) return;
            setBusy(true);
            askLog.replaceChildren();
            links.hidden = true;
            links.replaceChildren();
            result.hidden = true;
            var src = new EventSource(form.dataset.stream + '?q=' + encodeURIComponent(q));
            src.addEventListener('display', function(e) {
                var d = JSON.parse(e.data);
                var div = document.createElement('div');
                div.className = 'ask-display';
                div.textContent = d.text;
                askLog.prepend(div);
            });
            src.addEventListener('tool_call', function(e) {
                var d = JSON.parse(e.data);
                var div = document.createElement('div');
                div.className = 'ask-tool';
                div.textContent = '> ' + d.name + '(' + d.input + ')';
                askLog.prepend(div);
            });
            src.addEventListener('tool_result', function(e) {
                var d = JSON.parse(e.data);
                var pre = document.createElement('pre');
                pre.className = 'ask-result-preview';
                pre.textContent = d.output;
                askLog.prepend(pre);
                requestAnimationFrame(function() {
                    if (pre.scrollHeight > pre.clientHeight) {
                        var expand = document.createElement('a');
                        expand.className = 'ask-expand';
                        expand.textContent = 'expand';
                        expand.href = '#';
                        expand.addEventListener('click', function(ev) {
                            ev.preventDefault();
                            var expanded = pre.classList.toggle('expanded');
                            expand.textContent = expanded ? 'collapse' : 'expand';
                        });
                        pre.before(expand);
                    }
                });
            });
            src.addEventListener('done', function(e) {
                var d = JSON.parse(e.data);
                var a = document.createElement('a');
                a.href = d.url;
                a.textContent = d.url;
                links.replaceChildren(a);
                links.hidden = false;
                result.src = d.url;
                result.hidden = false;
                src.close();
                setBusy(false);
            });
            src.addEventListener('ask_error', function(e) {
                var d = JSON.parse(e.data);
                var div = document.createElement('div');
                div.className = 'ask-error';
                div.textContent = d.error;
                askLog.prepend(div);
                src.close();
                setBusy(false);
            });
            src.onerror = function() {
                src.close();
                setBusy(false);
            };
        });
    }
})();
