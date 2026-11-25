"use strict";
Object.defineProperty(exports, "__esModule", { value: true });
exports.activate = activate;
exports.deactivate = deactivate;
// The "Beautify Selected Code" command is withdrawn until PixCap ships a
// server for it.
//
// It sent the selection to a fixed endpoint (/tmp/pixcap.sock, or the
// \\.\pipe\pixcap named pipe) that the app never served. Any local process
// could create that endpoint first, read the user's code, and choose what was
// copied to the clipboard. It returns once a per-user endpoint and a server
// the app actually starts exist.
function activate(_context) { }
function deactivate() { }
//# sourceMappingURL=extension.js.map