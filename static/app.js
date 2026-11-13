// Page behaviour, kept out of the HTML so the Content-Security-Policy can
// forbid inline script. Each block runs only on pages carrying its element;
// URLs come from data attributes the server fills in. Embed URLs come from IRC
// lines, so embeds are built with createElement, never from an HTML string.
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

    var bindToggle = function(id, key, fallback, apply) {
        var box = document.getElementById(id);
        if (!box) return null;
        var stored = localStorage.getItem(key);
        box.checked = stored === null ? fallback : stored === '1';
        apply(box.checked);
        box.addEventListener('change', function() {
            localStorage.setItem(key, box.checked ? '1' : '0');
            apply(box.checked);
        });
        return box;
    };

    var IMAGE = /\.(png|jpe?g|gif|webp)$/i;
    var VIDEO = /\.(mp4|webm)$/i;
    var YOUTUBE_ID = /^[A-Za-z0-9_-]{11}$/;
    var YOUTUBE_HOSTS = ['youtube.com', 'www.youtube.com', 'm.youtube.com'];

    var youtubeId = function(url) {
        var id = null;
        if (url.hostname === 'youtu.be') {
            id = url.pathname.slice(1);
        } else if (YOUTUBE_HOSTS.indexOf(url.hostname) !== -1) {
            if (url.pathname === '/watch') {
                id = url.searchParams.get('v');
            } else if (url.pathname.indexOf('/shorts/') === 0) {
                id = url.pathname.slice('/shorts/'.length);
            }
        }
        return id !== null && YOUTUBE_ID.test(id) ? id : null;
    };

    var embedFor = function(href) {
        var url;
        try {
            url = new URL(href);
        } catch (e) {
            return null;
        }
        if (url.protocol !== 'https:') return null;
        var el;
        var src = url.href;
        var id = youtubeId(url);
        if (id) {
            el = document.createElement('iframe');
            el.className = 'embed-youtube';
            el.setAttribute('sandbox', 'allow-scripts allow-same-origin allow-presentation');
            el.setAttribute('allow', 'fullscreen');
            el.setAttribute('loading', 'lazy');
            src = 'https://www.youtube-nocookie.com/embed/' + id;
            // The player refuses to load without a Referer (error 153); send the origin only.
            el.referrerPolicy = 'strict-origin-when-cross-origin';
        } else if (IMAGE.test(url.pathname)) {
            el = document.createElement('img');
            el.setAttribute('loading', 'lazy');
            el.referrerPolicy = 'no-referrer';
        } else if (VIDEO.test(url.pathname)) {
            el = document.createElement('video');
            el.controls = true;
            el.preload = 'metadata';
            el.referrerPolicy = 'no-referrer';
        } else {
            return null;
        }
        el.src = src;
        return el;
    };

    var embedLine = function(line) {
        if (line.hasAttribute('data-embedded')) return;
        line.setAttribute('data-embedded', '');
        var box = document.createElement('div');
        box.className = 'embeds';
        var links = line.querySelectorAll('span.msg a[href], span.action a[href]');
        for (var i = 0; i < links.length; i++) {
            var el = embedFor(links[i].getAttribute('href'));
            if (el) box.appendChild(el);
        }
        if (box.firstChild) line.appendChild(box);
    };

    var unembedLine = function(line) {
        line.removeAttribute('data-embedded');
        var box = line.querySelector('div.embeds');
        if (box) box.remove();
    };

    var log = document.getElementById('log');
    var embeds = null;
    if (log) {
        var legacy = localStorage.getItem('hide-events');
        if (legacy !== null) {
            if (localStorage.getItem('show-events') === null) {
                localStorage.setItem('show-events', legacy === '1' ? '0' : '1');
            }
            localStorage.removeItem('hide-events');
        }
        bindToggle('toggle-live', 'live-status', true, function() {});
        bindToggle('toggle-events', 'show-events', false, function(on) {
            log.classList.toggle('hide-events', !on);
        });
        embeds = bindToggle('toggle-embeds', 'show-embeds', false, function(on) {
            var lines = log.querySelectorAll(on ? '.line' : '.line[data-embedded]');
            for (var i = 0; i < lines.length; i++) {
                if (on) {
                    embedLine(lines[i]);
                } else {
                    unembedLine(lines[i]);
                }
            }
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
            var added = log.lastElementChild;
            if (embeds && embeds.checked && added && added.classList.contains('line')) {
                embedLine(added);
            }
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
