// Flip Companion — KWin Script (Plasma 6)
//
// This script runs inside KWin and exposes a D-Bus interface that the
// Rust companion app calls to list windows and move them between outputs.
//
// Plasma 6 KWin scripting API:
//   workspace.windowList()           — all managed windows
//   window.caption                   — window title
//   window.output                    — the Output object
//   window.output.name               — output connector name
//   workspace.outputs                — array of Output objects
//   window.output = outputObj        — move window to output
//
// D-Bus interface registered at:
//   org.kde.KWin.Script.flipCompanionShuttle

// --- D-Bus callable functions ---

/**
 * List all managed windows with their id, caption, and output name.
 * Returns a JSON string: [{"id": "...", "caption": "...", "output": "..."}]
 */
function listWindows() {
    var windows = workspace.windowList();
    var result = [];
    for (var i = 0; i < windows.length; i++) {
        var w = windows[i];
        // Skip special windows (docks, panels, desktops)
        if (w.skipTaskbar || w.skipPager) {
            continue;
        }
        result.push({
            id: w.internalId.toString(),
            caption: w.caption,
            output: w.output ? w.output.name : ""
        });
    }
    return JSON.stringify(result);
}

/**
 * Move a window to a target output by name.
 * @param {string} windowId — the internalId of the window
 * @param {string} outputName — the connector name of the target output
 * Returns "ok" on success, or an error message.
 */
function moveWindowToOutput(windowId, outputName) {
    // Find the target output
    var targetOutput = null;
    var outputs = workspace.outputs;
    for (var i = 0; i < outputs.length; i++) {
        if (outputs[i].name === outputName) {
            targetOutput = outputs[i];
            break;
        }
    }
    if (!targetOutput) {
        return "error: output not found: " + outputName;
    }

    // Find the window
    var windows = workspace.windowList();
    for (var i = 0; i < windows.length; i++) {
        if (windows[i].internalId.toString() === windowId) {
            windows[i].output = targetOutput;
            return "ok";
        }
    }
    return "error: window not found: " + windowId;
}

/**
 * List all outputs with their names and geometry.
 * Returns a JSON string: [{"name": "...", "x": 0, "y": 0, "width": 1920, "height": 1080}]
 */
function listOutputs() {
    var outputs = workspace.outputs;
    var result = [];
    for (var i = 0; i < outputs.length; i++) {
        var o = outputs[i];
        var geo = o.geometry;
        result.push({
            name: o.name,
            x: geo.x,
            y: geo.y,
            width: geo.width,
            height: geo.height
        });
    }
    return JSON.stringify(result);
}

// Register the D-Bus interface so the Rust app can call these functions.
registerDBusObject(
    "/FlipCompanion",
    "org.kde.KWin.Script.flipCompanionShuttle",
    {
        listWindows: listWindows,
        moveWindowToOutput: moveWindowToOutput,
        listOutputs: listOutputs
    }
);
