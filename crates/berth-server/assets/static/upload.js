// Book upload: groups dropped files into books (one folder = one book, a loose audio file =
// one book), asks the server where each file goes, sends the files with progress and hands
// the upload over for analysis. The server renders the cards; this script only fills them.
(function () {
  'use strict';

  var zone = document.getElementById('dropzone');
  if (!zone) return;
  var queue = document.getElementById('queue');
  var notice = document.getElementById('drop-notice');
  var presigned = zone.dataset.presigned === 'true';
  var AUDIO = ['mp3', 'm4b', 'm4a', 'aac', 'ogg', 'opus', 'flac', 'wav'];
  var IMAGES = ['jpg', 'jpeg', 'png', 'webp'];

  function ext(name) {
    var dot = name.lastIndexOf('.');
    return dot < 0 ? '' : name.slice(dot + 1).toLowerCase();
  }

  // ── SHA-256 over a File, in chunks (presigned uploads are signed with it) ──
  var K = new Uint32Array([
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2]);

  function Sha256() {
    this.h = new Uint32Array([0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19]);
    this.buf = new Uint8Array(64);
    this.used = 0;
    this.length = 0;
    this.w = new Uint32Array(64);
  }
  Sha256.prototype.block = function (b, o) {
    var w = this.w, h = this.h, i;
    for (i = 0; i < 16; i++) w[i] = (b[o + i * 4] << 24) | (b[o + i * 4 + 1] << 16) | (b[o + i * 4 + 2] << 8) | b[o + i * 4 + 3];
    for (i = 16; i < 64; i++) {
      var x = w[i - 15], y = w[i - 2];
      var s0 = ((x >>> 7) | (x << 25)) ^ ((x >>> 18) | (x << 14)) ^ (x >>> 3);
      var s1 = ((y >>> 17) | (y << 15)) ^ ((y >>> 19) | (y << 13)) ^ (y >>> 10);
      w[i] = (w[i - 16] + s0 + w[i - 7] + s1) | 0;
    }
    var a = h[0], bb = h[1], c = h[2], d = h[3], e = h[4], f = h[5], g = h[6], hh = h[7];
    for (i = 0; i < 64; i++) {
      var S1 = ((e >>> 6) | (e << 26)) ^ ((e >>> 11) | (e << 21)) ^ ((e >>> 25) | (e << 7));
      var t1 = (hh + S1 + ((e & f) ^ (~e & g)) + K[i] + w[i]) | 0;
      var S0 = ((a >>> 2) | (a << 30)) ^ ((a >>> 13) | (a << 19)) ^ ((a >>> 22) | (a << 10));
      var t2 = (S0 + ((a & bb) ^ (a & c) ^ (bb & c))) | 0;
      hh = g; g = f; f = e; e = (d + t1) | 0; d = c; c = bb; bb = a; a = (t1 + t2) | 0;
    }
    h[0] += a; h[1] += bb; h[2] += c; h[3] += d; h[4] += e; h[5] += f; h[6] += g; h[7] += hh;
  };
  Sha256.prototype.update = function (data) {
    var i = 0;
    this.length += data.length;
    if (this.used) {
      while (this.used < 64 && i < data.length) this.buf[this.used++] = data[i++];
      if (this.used < 64) return;
      this.block(this.buf, 0);
      this.used = 0;
    }
    for (; i + 64 <= data.length; i += 64) this.block(data, i);
    while (i < data.length) this.buf[this.used++] = data[i++];
  };
  Sha256.prototype.hex = function () {
    var bits = this.length * 8;
    var pad = new Uint8Array(((this.used < 56) ? 56 : 120) - this.used + 8);
    pad[0] = 0x80;
    var hi = Math.floor(bits / 0x100000000), lo = bits >>> 0;
    var n = pad.length;
    pad[n - 8] = hi >>> 24; pad[n - 7] = hi >>> 16; pad[n - 6] = hi >>> 8; pad[n - 5] = hi;
    pad[n - 4] = lo >>> 24; pad[n - 3] = lo >>> 16; pad[n - 2] = lo >>> 8; pad[n - 1] = lo;
    this.length -= pad.length; // padding is not message
    this.update(pad);
    var out = '';
    for (var i = 0; i < 8; i++) out += ('00000000' + this.h[i].toString(16)).slice(-8);
    return out;
  };

  async function sha256(file, onProgress) {
    var hash = new Sha256();
    var step = 4 << 20;
    for (var at = 0; at < file.size; at += step) {
      hash.update(new Uint8Array(await file.slice(at, at + step).arrayBuffer()));
      onProgress(Math.min(file.size, at + step));
    }
    return hash.hex();
  }

  // ── Collecting files from a drop (folders included) or a picker ──
  function readEntry(entry, prefix, out) {
    return new Promise(function (resolve) {
      if (entry.isFile) {
        entry.file(function (file) { out.push({ path: prefix + file.name, file: file }); resolve(); }, function () { resolve(); });
        return;
      }
      var reader = entry.createReader();
      var all = [];
      (function next() {
        reader.readEntries(function (batch) {
          if (!batch.length) {
            Promise.all(all.map(function (e) { return readEntry(e, prefix + entry.name + '/', out); })).then(resolve);
            return;
          }
          all = all.concat(Array.prototype.slice.call(batch));
          next();
        }, function () { resolve(); });
      })();
    });
  }

  // One book per top folder; loose audio files are books of their own.
  function group(items) {
    var books = {};
    var order = [];
    items.forEach(function (item) {
      var kind = AUDIO.indexOf(ext(item.path)) >= 0 ? 'audio' : IMAGES.indexOf(ext(item.path)) >= 0 ? 'image' : null;
      if (!kind) return;
      var slash = item.path.indexOf('/');
      var key = slash > 0 ? item.path.slice(0, slash) : (kind === 'audio' ? item.path : '');
      if (!key) return; // a loose picture belongs to no book
      if (!books[key]) { books[key] = []; order.push(key); }
      books[key].push({ name: slash > 0 ? item.path.slice(slash + 1) : item.path, file: item.file });
    });
    return order.map(function (key) { return { name: key, files: books[key] }; })
      .filter(function (b) { return b.files.some(function (f) { return AUDIO.indexOf(ext(f.name)) >= 0; }); });
  }

  function show(text) {
    notice.textContent = text;
    notice.classList.toggle('hidden', !text);
  }

  async function html(url, options) {
    var response = await fetch(url, Object.assign({ headers: { 'hx-request': 'true' } }, options || {}));
    return response.text();
  }

  function place(id, markup) {
    var template = document.createElement('template');
    template.innerHTML = markup.trim();
    var card = template.content.firstElementChild;
    var old = document.getElementById('upload-' + id);
    if (old) old.replaceWith(card); else queue.prepend(card);
    var empty = document.getElementById('queue-empty');
    if (empty) empty.remove();
    if (window.htmx) window.htmx.process(card);
    return card;
  }

  function send(method, url, headers, file, onProgress) {
    return new Promise(function (resolve, reject) {
      var xhr = new XMLHttpRequest();
      xhr.open(method, url);
      Object.keys(headers).forEach(function (k) { xhr.setRequestHeader(k, headers[k]); });
      xhr.upload.onprogress = function (e) { onProgress(e.loaded); };
      xhr.onload = function () { (xhr.status < 300 ? resolve : reject)(new Error(xhr.status + ' ' + xhr.responseText)); };
      xhr.onerror = function () { reject(new Error('network')); };
      xhr.send(file);
    });
  }

  async function upload(book) {
    var total = book.files.reduce(function (s, f) { return s + f.file.size; }, 0);
    var files = book.files.map(function (f) { return { name: f.name, size: f.file.size }; });
    if (presigned) {
      var hashed = 0;
      show('');
      for (var i = 0; i < book.files.length; i++) {
        var base = hashed;
        files[i].sha256 = await sha256(book.files[i].file, function (done) {
          show(zone.dataset.textHashing + ' ' + book.name + ' · ' + Math.round((base + done) * 100 / total) + '%');
        });
        hashed += book.files[i].file.size;
      }
      show('');
    }
    var response = await fetch('/upload/new', {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ name: book.name, files: files }),
    });
    if (!response.ok) {
      var problem = await response.json().catch(function () { return {}; });
      show(zone.dataset.textFailed + ' ' + book.name + (problem.detail ? ' (' + problem.detail + ')' : ''));
      return;
    }
    var created = await response.json();
    var card = place(created.id, await html('/upload/' + created.id));
    var bar = card.querySelector('[data-progress]');
    var status = card.querySelector('[data-status]');
    var sent = 0;
    try {
      for (var j = 0; j < book.files.length; j++) {
        var target = created.targets[j];
        var file = book.files[j].file;
        var before = sent;
        var progress = function (loaded) {
          var pct = Math.round((before + loaded) * 100 / Math.max(total, 1));
          if (bar) bar.style.width = pct + '%';
          if (status) status.textContent = zone.dataset.textSending + ' ' + pct + '%';
        };
        if (target.method === 'server') {
          await send('PUT', '/upload/' + created.id + '/files/' + j, { 'content-type': 'application/octet-stream' }, file, progress);
        } else if (target.method === 'presigned') {
          var headers = {};
          // Browsers set Content-Length themselves; the signature covers the same value.
          target.headers.forEach(function (h) { if (h[0] !== 'content-length') headers[h[0]] = h[1]; });
          await send('PUT', target.url, headers, file, progress);
        }
        sent += file.size;
        progress(0);
      }
    } catch (err) {
      if (status) status.textContent = zone.dataset.textFailed + ' ' + err.message;
      return;
    }
    place(created.id, await html('/upload/' + created.id + '/complete', { method: 'POST' }));
  }

  var busy = Promise.resolve();
  function accept(items) {
    var books = group(items);
    if (!books.length) { show(zone.dataset.textNothing); return; }
    show('');
    books.forEach(function (book) { busy = busy.then(function () { return upload(book); }).catch(function (e) { show(String(e)); }); });
  }

  zone.addEventListener('dragover', function (e) { e.preventDefault(); zone.dataset.over = 'true'; });
  zone.addEventListener('dragleave', function () { zone.dataset.over = 'false'; });
  zone.addEventListener('drop', function (e) {
    e.preventDefault();
    zone.dataset.over = 'false';
    var out = [];
    var entries = Array.prototype.map.call(e.dataTransfer.items, function (i) { return i.webkitGetAsEntry && i.webkitGetAsEntry(); }).filter(Boolean);
    Promise.all(entries.map(function (entry) { return readEntry(entry, '', out); })).then(function () { accept(out); });
  });
  ['pick-folder', 'pick-files'].forEach(function (id) {
    var input = document.getElementById(id);
    input.addEventListener('change', function () {
      accept(Array.prototype.map.call(input.files, function (f) { return { path: f.webkitRelativePath || f.name, file: f }; }));
      input.value = '';
    });
  });
})();
