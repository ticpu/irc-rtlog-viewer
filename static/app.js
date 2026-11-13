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
        bindToggle('toggle-live', 'live-status', true, function(on) {
            log.classList.toggle('hide-live', !on);
        });
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

    // Times are shown in the viewer's zone: the browser's, or the one picked
    // in tz-select and kept in localStorage. Line anchors stay in file time.
    var stampFormat = null;
    var partsFormat = null;
    var onZoneChange = [];
    var useZone = function(zone) {
        var options = zone ? {timeZone: zone} : {};
        try {
            stampFormat = new Intl.DateTimeFormat(undefined,
                Object.assign({dateStyle: 'medium', timeStyle: 'medium'}, options));
            partsFormat = new Intl.DateTimeFormat('en-CA', Object.assign({
                year: 'numeric', month: '2-digit', day: '2-digit',
                hour: '2-digit', minute: '2-digit', second: '2-digit', hourCycle: 'h23'
            }, options));
            return true;
        } catch (e) {
            useZone(null);
            return false;
        }
    };
    var when = function(stamp) {
        return stampFormat.format(new Date(stamp));
    };
    // A line's time as hh:mm:ss in the viewer's zone, with MM-DD in front when
    // that falls on another day than the file the line is from.
    var showLineTimes = function(root) {
        var links = root.querySelectorAll('a.ts[data-at]');
        for (var i = 0; i < links.length; i++) {
            var parts = {};
            partsFormat.formatToParts(new Date(links[i].getAttribute('data-at'))).forEach(function(p) {
                parts[p.type] = p.value;
            });
            var dayOf = links[i].closest('[data-date]');
            var fileDate = dayOf ? dayOf.getAttribute('data-date') : null;
            var date = parts.year + '-' + parts.month + '-' + parts.day;
            links[i].textContent = (fileDate && date !== fileDate ? parts.month + '-' + parts.day + ' ' : '') +
                parts.hour + ':' + parts.minute + ':' + parts.second;
        }
    };

    var zone = localStorage.getItem('tz');
    if (!useZone(zone)) {
        console.warn('dropping unknown time zone ' + zone);
        localStorage.removeItem('tz');
        zone = null;
    }
    var tzSelect = document.getElementById('tz-select');
    if (tzSelect) {
        var browserZone = Intl.DateTimeFormat().resolvedOptions().timeZone;
        tzSelect.options[0].textContent = 'browser time zone (' + browserZone + ')';
        var zones = typeof Intl.supportedValuesOf === 'function' ? Intl.supportedValuesOf('timeZone') : [];
        if (zones.indexOf('UTC') === -1) zones = ['UTC'].concat(zones);
        if (zone && zones.indexOf(zone) === -1) zones = [zone].concat(zones);
        for (var z = 0; z < zones.length; z++) {
            var option = document.createElement('option');
            option.value = zones[z];
            option.textContent = zones[z];
            tzSelect.appendChild(option);
        }
        tzSelect.value = zone || '';
        tzSelect.addEventListener('change', function() {
            if (tzSelect.value) {
                localStorage.setItem('tz', tzSelect.value);
            } else {
                localStorage.removeItem('tz');
            }
            useZone(tzSelect.value || null);
            onZoneChange.forEach(function(render) {
                render();
            });
        });
    }
    if (log) {
        showLineTimes(log);
        onZoneChange.push(function() {
            showLineTimes(log);
        });
    }

    // The JSON keys users by display nick; lookups go through the network's
    // rfc1459 casemapping, as the server's index does.
    var RFC1459_UPPER = {'[': '{', ']': '}', '\\': '|', '~': '^'};
    var casemap = function(nick) {
        return nick.replace(/[A-Z[\]\\~]/g, function(c) {
            return RFC1459_UPPER[c] || c.toLowerCase();
        });
    };

    // Called with each line the tail appends; set when presence is on.
    var onNewLine = null;
    if (log && log.dataset.presence) {
        var presenceUrl = log.dataset.presence;
        var topicBox = document.getElementById('topic');
        var users = {};
        var topic = null;
        var modes = null;
        var scanning = true;
        var snapshotAt = null;
        var stale = false;

        // In the channel now: true or false with a ZNC snapshot, null without one.
        var liveOf = function(user) {
            if (snapshotAt === null) return null;
            return user ? user.live === true : false;
        };
        var hoverText = function(nick) {
            var user = users[casemap(nick)];
            var lines = [];
            var live = liveOf(user);
            if (live !== null) {
                lines.push((live ? 'in the channel now' : 'not in the channel now') +
                    (stale ? ' (as of ' + when(snapshotAt) + ', may be out of date)' : ''));
            }
            if (!user) {
                lines.push(scanning ? 'archive scan in progress' : 'not seen in the archive');
            } else {
                if (user.online) lines.push('last online: ' + when(user.online));
                if (user.offline) lines.push('last offline: ' + when(user.offline));
                if (user.spoke) lines.push('last spoke: ' + when(user.spoke));
            }
            return lines.join('\n');
        };
        // A dot and the member prefixes, placed just before the nick.
        var mark = function(nickEl, user, title) {
            var marker = nickEl.previousElementSibling;
            if (!marker || !marker.classList.contains('presence')) marker = null;
            var live = liveOf(user);
            if (live === null) {
                if (marker) marker.remove();
                return;
            }
            if (!marker) {
                marker = document.createElement('span');
                nickEl.before(marker);
            }
            marker.className = 'presence ' + (live ? 'online' : 'offline') + (stale ? ' stale' : '');
            marker.textContent = (live ? '●' : '○') + (live && user.prefixes ? user.prefixes : '');
            marker.title = title;
        };
        var decorate = function(root) {
            var nicks = root.querySelectorAll('span.nick[data-nick]');
            for (var i = 0; i < nicks.length; i++) {
                var nick = nicks[i].getAttribute('data-nick');
                var title = hoverText(nick);
                nicks[i].title = title;
                mark(nicks[i], users[casemap(nick)], title);
            }
        };
        var showTopic = function() {
            if (!topicBox) return;
            var parts = [];
            if (modes) parts.push('Modes: ' + modes);
            if (topic) parts.push('Topic: ' + topic.text + ' (set by ' + topic.nick + ', ' + when(topic.at) + ')');
            topicBox.textContent = parts.join(' · ');
            topicBox.hidden = parts.length === 0;
        };
        var render = function() {
            decorate(log);
            showTopic();
        };
        onZoneChange.push(render);

        var timer = null;
        var load;
        var schedule = function(ms) {
            if (timer !== null) return;
            timer = setTimeout(function() {
                timer = null;
                load();
            }, ms);
        };
        // no-cache revalidates with the ETag; max-age alone would serve
        // a stale copy for 30 s after a line arrived.
        load = function() {
            fetch(presenceUrl, {cache: 'no-cache'}).then(function(resp) {
                if (!resp.ok) throw new Error('HTTP ' + resp.status);
                return resp.json();
            }).then(function(body) {
                users = {};
                Object.keys(body.users).forEach(function(nick) {
                    users[casemap(nick)] = body.users[nick];
                });
                topic = body.topic;
                modes = body.modes;
                scanning = body.scanning;
                snapshotAt = body.snapshot_at;
                stale = body.stale;
                render();
                if (scanning) schedule(5000);
            }).catch(function(e) {
                console.warn('presence: ' + presenceUrl + ': ' + e.message);
            });
        };
        load();
        onNewLine = function(line) {
            decorate(line);
            schedule(5000);
        };
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
            if (added && added.classList.contains('line')) {
                showLineTimes(added);
                if (embeds && embeds.checked) embedLine(added);
                if (onNewLine) onNewLine(added);
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
