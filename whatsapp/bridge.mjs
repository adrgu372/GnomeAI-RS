#!/usr/bin/env node

import fs from 'fs';
import http from 'http';
import path from 'path';

import pino from 'pino';
import makeWASocket, {
  Browsers,
  DisconnectReason,
  downloadMediaMessage,
  fetchLatestWaWebVersion,
  makeCacheableSignalKeyStore,
  normalizeMessageContent,
  useMultiFileAuthState,
} from '@whiskeysockets/baileys';

const apiBase = process.env.GNOME_API_BASE || 'http://127.0.0.1:8787';
const authDir =
  process.env.GNOME_WA_AUTH_DIR || path.resolve('store/whatsapp/auth');
const bridgePort = Number(process.env.GNOME_WA_BRIDGE_PORT || 8788);
const assistantName = process.env.GNOME_WA_ASSISTANT_NAME || 'Gnome AI';
const webToken = process.env.GNOMEF_WEB_TOKEN || '';
const hasOwnNumber = process.env.GNOME_WA_HAS_OWN_NUMBER === '1';
const ownerPid = process.ppid;
const maxInboundMediaBytes = Number(
  process.env.GNOME_WA_MAX_MEDIA_BYTES || 15 * 1024 * 1024,
);

const logger = pino({
  level: process.env.GNOME_WA_LOG_LEVEL || 'warn',
});

let sock;
let connected = false;
let shuttingDown = false;
let reconnectTimer = null;
let flushing = false;
const outgoingQueue = [];
const lidToPhoneMap = {};

const state = {
  bridge_running: true,
  connected: false,
  authenticated: false,
  qr: '',
  own_jid: '',
  own_phone: '',
  assistant_name: assistantName,
  has_own_number: hasOwnNumber,
  queue_size: 0,
  last_error: '',
};

function clearOwnIdentity() {
  state.own_jid = '';
  state.own_phone = '';
}

function disconnectErrorText(error) {
  const candidates = [
    error?.output?.payload?.message,
    error?.data?.message,
    error?.message,
  ];
  return candidates
    .map((value) => String(value || '').trim())
    .find(Boolean) || '';
}

function writeJson(res, status, payload) {
  res.writeHead(status, { 'Content-Type': 'application/json; charset=utf-8' });
  res.end(JSON.stringify(payload));
}

async function readJsonBody(req) {
  const chunks = [];
  for await (const chunk of req) chunks.push(chunk);
  const raw = Buffer.concat(chunks).toString('utf8').trim();
  return raw ? JSON.parse(raw) : {};
}

function scheduleReconnect(delayMs = 5000) {
  if (shuttingDown || reconnectTimer) return;
  reconnectTimer = setTimeout(() => {
    reconnectTimer = null;
    connectInternal().catch((err) => {
      state.last_error = err?.message || String(err);
      logger.error({ err }, 'WhatsApp reconnect failed');
      scheduleReconnect(5000);
    });
  }, delayMs);
}

async function latestWaVersion() {
  try {
    return await Promise.race([
      fetchLatestWaWebVersion({}),
      new Promise((_, reject) => {
        setTimeout(
          () => reject(new Error('WhatsApp version lookup timed out')),
          5000,
        );
      }),
    ]);
  } catch (err) {
    logger.warn({ err }, 'Failed to fetch WA Web version, using default');
    return { version: undefined };
  }
}

async function resetAuthState() {
  try {
    fs.rmSync(authDir, { recursive: true, force: true });
  } catch (err) {
    logger.warn({ err }, 'Failed to remove WhatsApp auth dir');
  }
  fs.mkdirSync(authDir, { recursive: true });
}

async function translateJid(jid) {
  if (!jid || !jid.endsWith('@lid')) return jid;
  const lidUser = jid.split('@')[0].split(':')[0];
  const cached = lidToPhoneMap[lidUser];
  if (cached) return cached;

  try {
    const pn = await sock?.signalRepository?.lidMapping?.getPNForLID(jid);
    if (pn) {
      const phoneJid = `${pn.split('@')[0].split(':')[0]}@s.whatsapp.net`;
      lidToPhoneMap[lidUser] = phoneJid;
      return phoneJid;
    }
  } catch (err) {
    logger.debug({ err, jid }, 'LID translation failed');
  }
  return jid;
}

function updateOwnIdentity() {
  if (!sock?.user?.id) return;
  const phoneUser = sock.user.id.split(':')[0].split('@')[0];
  const lidUser = sock.user.lid?.split(':')[0];
  if (lidUser && phoneUser) {
    lidToPhoneMap[lidUser] = `${phoneUser}@s.whatsapp.net`;
  }
  state.own_phone = phoneUser || '';
  state.own_jid = phoneUser ? `${phoneUser}@s.whatsapp.net` : '';
}

async function flushOutgoingQueue() {
  if (!connected || flushing || outgoingQueue.length === 0) return;
  flushing = true;
  try {
    while (outgoingQueue.length > 0) {
      const item = outgoingQueue.shift();
      state.queue_size = outgoingQueue.length;
      await sock.sendMessage(item.jid, { text: item.text });
    }
  } finally {
    flushing = false;
  }
}

async function sendMessage(jid, text) {
  const outbound = hasOwnNumber ? text : `${assistantName}: ${text}`;
  if (!connected) {
    outgoingQueue.push({ jid, text: outbound });
    state.queue_size = outgoingQueue.length;
    return { queued: true };
  }
  try {
    await sock.sendMessage(jid, { text: outbound });
    state.queue_size = outgoingQueue.length;
    return { queued: false };
  } catch (err) {
    outgoingQueue.push({ jid, text: outbound });
    state.queue_size = outgoingQueue.length;
    logger.warn({ err, jid }, 'WhatsApp send failed, message queued');
    return { queued: true };
  }
}

async function forwardInbound(payload) {
  try {
    const resp = await fetch(`${apiBase}/api/whatsapp/inbound`, {
      method: 'POST',
      headers: {
        'Content-Type': 'application/json',
        'X-Gnomef-Token': webToken,
      },
      body: JSON.stringify(payload),
    });
    if (!resp.ok) {
      const text = await resp.text();
      logger.warn(
        { status: resp.status, text: text.slice(0, 200) },
        'Inbound delivery failed',
      );
    }
  } catch (err) {
    logger.warn({ err }, 'Failed to forward inbound WhatsApp message');
  }
}

function extensionFromMime(mimetype) {
  const clean = String(mimetype || '').split(';')[0].trim().toLowerCase();
  const map = {
    'image/jpeg': 'jpg',
    'image/jpg': 'jpg',
    'image/png': 'png',
    'image/webp': 'webp',
    'image/gif': 'gif',
    'image/bmp': 'bmp',
    'application/pdf': 'pdf',
    'application/msword': 'doc',
    'application/vnd.openxmlformats-officedocument.wordprocessingml.document': 'docx',
    'application/vnd.ms-excel': 'xls',
    'application/vnd.openxmlformats-officedocument.spreadsheetml.sheet': 'xlsx',
    'application/vnd.ms-powerpoint': 'ppt',
    'application/vnd.openxmlformats-officedocument.presentationml.presentation': 'pptx',
    'application/vnd.oasis.opendocument.text': 'odt',
    'application/vnd.oasis.opendocument.spreadsheet': 'ods',
    'application/vnd.oasis.opendocument.presentation': 'odp',
    'application/rtf': 'rtf',
    'application/json': 'json',
    'application/xml': 'xml',
    'application/zip': 'zip',
    'text/plain': 'txt',
    'text/markdown': 'md',
    'text/csv': 'csv',
    'text/html': 'html',
    'text/xml': 'xml',
    'text/x-python': 'py',
    'text/javascript': 'js',
    'text/x-rust': 'rs',
    'text/x-c': 'c',
    'text/x-csrc': 'c',
    'text/x-c++src': 'cpp',
    'text/x-java': 'java',
    'text/x-shellscript': 'sh',
    'audio/ogg': 'ogg',
    'audio/opus': 'opus',
    'audio/mpeg': 'mp3',
    'audio/mp4': 'm4a',
    'audio/aac': 'aac',
    'audio/wav': 'wav',
    'audio/x-wav': 'wav',
    'audio/flac': 'flac',
    'audio/amr': 'amr',
    'video/mp4': 'mp4',
    'video/quicktime': 'mov',
    'video/webm': 'webm',
    'video/x-matroska': 'mkv',
    'video/3gpp': '3gp',
    'image/webp': 'webp',
  };
  if (map[clean]) return map[clean];
  if (clean.startsWith('text/')) return 'txt';
  if (clean.startsWith('audio/')) return 'ogg';
  if (clean.startsWith('video/')) return 'mp4';
  if (clean.startsWith('image/')) return 'jpg';
  return 'bin';
}

// WhatsApp wraps captioned documents and view-once media in their own
// envelopes; normalizeMessageContent unwraps most, these are the rest.
function mediaPayload(normalized) {
  const document =
    normalized.documentMessage ||
    normalized.documentWithCaptionMessage?.message?.documentMessage;
  if (document) return { payload: document, kind: 'document' };
  if (normalized.imageMessage) {
    return { payload: normalized.imageMessage, kind: 'image' };
  }
  // Stickers are WebP images; treating them as images gives OCR and vision
  // for free.
  if (normalized.stickerMessage) {
    return { payload: normalized.stickerMessage, kind: 'sticker' };
  }
  // Voice notes (PTT) and regular audio share audioMessage.
  if (normalized.audioMessage) {
    return { payload: normalized.audioMessage, kind: 'audio' };
  }
  if (normalized.videoMessage) {
    return { payload: normalized.videoMessage, kind: 'video' };
  }
  return { payload: null, kind: null };
}

function defaultMime(kind) {
  switch (kind) {
    case 'image':
      return 'image/jpeg';
    case 'sticker':
      return 'image/webp';
    case 'audio':
      return 'audio/ogg';
    case 'video':
      return 'video/mp4';
    default:
      return 'application/octet-stream';
  }
}

function inboundMediaPrompt(media) {
  switch (media.kind) {
    case 'image':
      return 'Analizeaza imaginea atasata.';
    case 'sticker':
      return 'Am primit un sticker. Spune-mi ce reprezinta.';
    case 'audio':
      return media.seconds
        ? `Am primit un mesaj vocal de ${media.seconds} secunde. Asculta-l si raspunde-mi.`
        : 'Am primit un mesaj vocal. Asculta-l si raspunde-mi.';
    case 'video':
      return media.seconds
        ? `Am primit un video de ${media.seconds} secunde. Spune-mi despre ce este.`
        : 'Am primit un video. Spune-mi despre ce este.';
    default:
      return `Analizeaza fisierul atasat: ${media.filename}`;
  }
}

async function extractInboundMedia(msg, normalized) {
  const { payload, kind } = mediaPayload(normalized);
  if (!payload) return null;

  const buffer = await downloadMediaMessage(
    msg,
    'buffer',
    {},
    {
      logger,
      reuploadRequest: sock.updateMediaMessage,
    },
  );

  if (!buffer || buffer.length === 0) return null;
  if (buffer.length > maxInboundMediaBytes) {
    throw new Error(
      `attachment too large: ${buffer.length} bytes (limit ${maxInboundMediaBytes})`,
    );
  }

  const mimetype = payload.mimetype || defaultMime(kind);
  const ext = extensionFromMime(mimetype);
  const nameStem = kind === 'document' ? 'file' : kind;
  const fallbackName = `whatsapp-${nameStem}-${msg.key.id || Date.now()}.${ext}`;
  // Stickers ride the image pipeline; everything else keeps its own kind so
  // the backend can pick OCR, transcription, or document extraction.
  const type = kind === 'sticker' ? 'image' : kind;
  const media = {
    type,
    kind,
    mimetype,
    filename: payload.fileName || fallbackName,
    caption: payload.caption || '',
    seconds: Number(payload.seconds || 0) || 0,
    data_base64: Buffer.from(buffer).toString('base64'),
  };
  // WhatsApp ships a still frame with every video; it gives a vision model
  // something to look at without any local demuxing.
  if (kind === 'video' && payload.jpegThumbnail?.length) {
    media.thumbnail_base64 = Buffer.from(payload.jpegThumbnail).toString(
      'base64',
    );
  }
  return media;
}

async function connectInternal() {
  fs.mkdirSync(authDir, { recursive: true });
  const { state: authState, saveCreds } = await useMultiFileAuthState(authDir);
  state.authenticated = Boolean(authState?.creds?.registered);

  const { version } = await latestWaVersion();

  sock = makeWASocket({
    version,
    auth: {
      creds: authState.creds,
      keys: makeCacheableSignalKeyStore(authState.keys, logger),
    },
    printQRInTerminal: false,
    logger,
    browser: Browsers.macOS('Chrome'),
  });

  sock.ev.on('creds.update', saveCreds);

  sock.ev.on('connection.update', (update) => {
    const { connection, lastDisconnect, qr } = update;

    if (qr) {
      state.qr = qr;
      state.connected = false;
      state.authenticated = false;
      state.last_error = '';
    }

    if (connection === 'open') {
      connected = true;
      state.connected = true;
      state.authenticated = true;
      state.qr = '';
      state.last_error = '';
      updateOwnIdentity();
      sock.sendPresenceUpdate('available').catch(() => {});
      flushOutgoingQueue().catch((err) => {
        logger.warn({ err }, 'Failed to flush queued WhatsApp messages');
      });
      return;
    }

    if (connection === 'close') {
      connected = false;
      state.connected = false;
      const reason = lastDisconnect?.error?.output?.statusCode;
      if (shuttingDown) return;
      if (reason === DisconnectReason.loggedOut) {
        state.last_error = 'logged_out';
        state.authenticated = false;
        state.qr = '';
        clearOwnIdentity();
        resetAuthState()
          .then(() => scheduleReconnect(1000))
          .catch((err) => {
            state.last_error = err?.message || String(err);
            logger.warn({ err }, 'Failed to reset auth state after logout');
            scheduleReconnect(5000);
        });
        return;
      }
      const detail = disconnectErrorText(lastDisconnect?.error);
      state.last_error = `disconnected:${reason || 'unknown'}${
        detail ? ` (${detail})` : ''
      }`;
      scheduleReconnect(5000);
    }
  });

  sock.ev.on('messages.upsert', async ({ messages }) => {
    for (const msg of messages) {
      if (!msg.message) continue;
      const normalized = normalizeMessageContent(msg.message);
      if (!normalized) continue;

      const rawJid = msg.key.remoteJid;
      if (!rawJid || rawJid === 'status@broadcast') continue;

      let media = null;
      try {
        media = await extractInboundMedia(msg, normalized);
      } catch (err) {
        logger.warn({ err }, 'Failed to download inbound WhatsApp media');
      }

      const content =
        normalized.conversation ||
        normalized.extendedTextMessage?.text ||
        normalized.imageMessage?.caption ||
        normalized.videoMessage?.caption ||
        media?.caption ||
        (media ? inboundMediaPrompt(media) : '') ||
        '';

      if (!content && !media) continue;

      const chatJid = await translateJid(rawJid);
      const sender = msg.key.participant || msg.key.remoteJid || '';
      const senderName = msg.pushName || sender.split('@')[0] || 'WhatsApp';
      const fromMe = Boolean(msg.key.fromMe);
      const isBotMessage = hasOwnNumber
        ? fromMe
        : content.startsWith(`${assistantName}:`);

      await forwardInbound({
        id: msg.key.id || '',
        chat_jid: chatJid,
        sender,
        sender_name: senderName,
        content,
        timestamp: new Date(
          Number(msg.messageTimestamp || Date.now() / 1000) * 1000,
        ).toISOString(),
        is_from_me: fromMe,
        is_bot_message: isBotMessage,
        own_jid: state.own_jid,
        own_phone: state.own_phone,
        media,
      });
    }
  });
}

async function gracefulShutdown() {
  shuttingDown = true;
  connected = false;
  state.connected = false;
  if (reconnectTimer) {
    clearTimeout(reconnectTimer);
    reconnectTimer = null;
  }
  try {
    sock?.end(undefined);
  } catch {}
  await new Promise((resolve) => server.close(resolve));
}

const server = http.createServer(async (req, res) => {
  const url = new URL(req.url || '/', `http://127.0.0.1:${bridgePort}`);
  const suppliedToken = String(req.headers['x-gnomef-token'] || '');
  if (!webToken || suppliedToken !== webToken) {
    writeJson(res, 401, { ok: false, error: 'invalid bridge token' });
    return;
  }

  if (req.method === 'GET' && url.pathname === '/status') {
    state.queue_size = outgoingQueue.length;
    writeJson(res, 200, state);
    return;
  }

  if (req.method === 'POST' && url.pathname === '/send') {
    try {
      const body = await readJsonBody(req);
      const jid = String(body.jid || '').trim();
      const text = String(body.text || '').trim();
      if (!jid || !text) {
        writeJson(res, 400, { ok: false, error: 'jid and text required' });
        return;
      }
      const result = await sendMessage(jid, text);
      writeJson(res, 200, { ok: true, ...result });
    } catch (err) {
      writeJson(res, 500, { ok: false, error: err?.message || String(err) });
    }
    return;
  }

  if (req.method === 'POST' && url.pathname === '/shutdown') {
    writeJson(res, 200, { ok: true });
    gracefulShutdown()
      .then(() => process.exit(0))
      .catch(() => process.exit(1));
    return;
  }

  writeJson(res, 404, { ok: false, error: 'not found' });
});

server.listen(bridgePort, '127.0.0.1', () => {
  logger.info({ bridgePort }, 'Gnome WhatsApp bridge listening');
});

// Do not leave an orphan bridge holding the API port when WebTool is stopped
// externally or crashes before it can call /shutdown.
const ownerWatchdog = setInterval(() => {
  if (shuttingDown || (ownerPid > 1 && process.ppid === ownerPid)) return;
  logger.warn({ ownerPid, currentParentPid: process.ppid }, 'WebTool parent exited');
  gracefulShutdown()
    .then(() => process.exit(0))
    .catch(() => process.exit(1));
}, 1000);
ownerWatchdog.unref();

connectInternal().catch((err) => {
  state.last_error = err?.message || String(err);
  logger.error({ err }, 'Initial WhatsApp connect failed');
  scheduleReconnect(5000);
});

process.on('SIGTERM', () => {
  gracefulShutdown()
    .then(() => process.exit(0))
    .catch(() => process.exit(1));
});

process.on('SIGINT', () => {
  gracefulShutdown()
    .then(() => process.exit(0))
    .catch(() => process.exit(1));
});
