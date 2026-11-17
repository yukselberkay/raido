// Raido's clipboard watcher (macOS, JavaScript for Automation). Terminal.app's ⌘V
// pastes only text, and the agents attach a pasted image path, so while Raido has
// the front, a screenshot (an image with no text) on the clipboard is saved as a
// PNG in `argv[0]` and its path is added beside the image as text. Prints
// "image <path>" for each one.
//
// Other apps never see that: when another app comes to the front, and when the
// watcher stops, the clipboard gets back exactly what was copied, so a browser
// (GitLab, say) is handed the same screenshot as without Raido.
//
// Raido writes a line to stdin when its window gains ("1") or loses ("0") the
// focus, and closes it to stop the watcher. `argv[1]` lists the processes Raido
// runs under and `argv[2]` names its terminal app: Raido has the front only while
// one of them is the frontmost app (neither known: Raido's focus alone decides).
// `argv[3]` names a private pasteboard instead of the system clipboard (tests).
// It also stops when Raido does: its parent process changes when Raido is gone.
ObjC.import('AppKit');
ObjC.import('CoreFoundation');
ObjC.import('unistd');

// Wakes the loop below when Raido writes, and when an app comes to the front (the
// run loop would otherwise sleep on past them).
var heard = false;
ObjC.registerSubclass({
  name: 'RaidoClipboardWake',
  methods: {
    'heard:': {
      types: ['void', ['id']],
      implementation: function () {
        heard = true;
        $.CFRunLoopStop($.CFRunLoopGetCurrent());
      },
    },
    'fronted:': {
      types: ['void', ['id']],
      implementation: function () { $.CFRunLoopStop($.CFRunLoopGetCurrent()); },
    },
  },
});

var TEXT = ['public.utf8-plain-text', 'public.plain-text', 'NSStringPboardType', 'public.file-url'];
var IMAGES = ['public.png', 'public.tiff', 'public.jpeg', 'public.heic', 'com.compuserve.gif'];

// The focus Raido told of last on stdin: true, false, or null once it's closed.
function told(input, focus) {
  // Nothing to read is the end (NSData's `length` is JavaScript's own here).
  var text = ObjC.unwrap($.NSString.alloc.initWithDataEncoding(input.availableData, $.NSUTF8StringEncoding));
  if (!text) {
    return null;
  }
  var lines = text.split('\n').filter(Boolean);
  return lines.length ? lines[lines.length - 1].trim() !== '0' : focus;
}

// Whether the frontmost app is the terminal Raido runs in.
function front(pids, bundle) {
  if (!pids.length && !bundle) {
    return true;
  }
  var app = $.NSWorkspace.sharedWorkspace.frontmostApplication;
  if (app.isNil()) {
    return true;
  }
  return pids.indexOf(app.processIdentifier) >= 0
    || (!!bundle && ObjC.unwrap(app.bundleIdentifier) === bundle);
}

// The clipboard's items, each a list of [type, data]: a screenshot (an image and
// no text), else null.
function screenshot(pb) {
  var types = ObjC.deepUnwrap(pb.types) || [];
  var hasText = types.some(function (t) { return TEXT.indexOf(t) >= 0; });
  var hasImage = types.some(function (t) { return IMAGES.indexOf(t) >= 0; });
  if (!hasImage || hasText) {
    return null;
  }
  var items = [];
  var list = pb.pasteboardItems;
  for (var i = 0; i < list.count; i++) {
    var item = list.objectAtIndex(i);
    var pairs = [];
    (ObjC.deepUnwrap(item.types) || []).forEach(function (t) {
      var data = item.dataForType(t);
      if (!data.isNil()) {
        pairs.push([t, data]);
      }
    });
    items.push(pairs);
  }
  return items;
}

// The screenshot as PNG data.
function png(items) {
  var pairs = [].concat.apply([], items);
  var image = IMAGES.map(function (t) {
    return pairs.filter(function (pair) { return pair[0] === t; })[0];
  }).filter(Boolean)[0];
  if (image[0] === 'public.png') {
    return image[1];
  }
  var rep = $.NSBitmapImageRep.imageRepWithData(image[1]);
  return rep.representationUsingTypeProperties($.NSBitmapImageFileTypePNG, $({}));
}

// Put `items` on the clipboard; its change count after.
function write(pb, items) {
  var objects = $.NSMutableArray.array;
  items.forEach(function (pairs) {
    var item = $.NSPasteboardItem.alloc.init;
    pairs.forEach(function (pair) { item.setDataForType(pair[1], pair[0]); });
    objects.addObject(item);
  });
  pb.clearContents;
  pb.writeObjects(objects);
  return pb.changeCount;
}

// The screenshot as copied, with a PNG of it (agents read that) and its path.
function withPath(shot) {
  var first = shot.items[0].filter(function (pair) { return pair[0] !== 'public.png'; });
  first.push(['public.png', shot.png]);
  first.push(['public.utf8-plain-text', $(shot.path).dataUsingEncoding($.NSUTF8StringEncoding)]);
  return [first].concat(shot.items.slice(1));
}

function run(argv) {
  var keep = argv[0];
  var pids = (argv[1] || '').split(',').filter(Boolean).map(Number);
  var bundle = argv[2] || '';
  var pb = argv[3]
    ? $.NSPasteboard.pasteboardWithName(argv[3])
    : $.NSPasteboard.generalPasteboard;
  var parent = $.getppid();
  var wake = $.RaidoClipboardWake.alloc.init;
  var input = $.NSFileHandle.fileHandleWithStandardInput;
  $.NSNotificationCenter.defaultCenter.addObserverSelectorNameObject(
    wake, 'heard:', $.NSFileHandleDataAvailableNotification, input);
  input.waitForDataInBackgroundAndNotify;
  if (pids.length || bundle) {
    $.NSWorkspace.sharedWorkspace.notificationCenter.addObserverSelectorNameObject(
      wake, 'fronted:', $.NSWorkspaceDidActivateApplicationNotification, $());
  }
  var focus = true;   // Raido's window has the focus
  var last = -1;      // the clipboard's change count looked at last
  var shot = null;    // the screenshot on it: {items, png, path}
  var shown = false;  // whether its path is on the clipboard beside it
  while ($.getppid() === parent) {
    if (heard) {
      heard = false;
      focus = told(input, focus);
      if (focus === null) {
        break;
      }
      input.waitForDataInBackgroundAndNotify;
    }
    var active = focus && front(pids, bundle);
    try {
      // A new clipboard matters once Raido has the front (or to keep from
      // putting the screenshot back over it).
      if ((active || shown) && pb.changeCount !== last) {
        last = pb.changeCount;
        shown = false;
        var items = screenshot(pb);
        shot = items ? { items: items, png: null, path: null } : null;
      }
      if (shot && active !== shown && pb.changeCount === last) {
        if (active && !shot.path) {
          shot.png = png(shot.items);
          var path = keep + '/clipboard-' + Date.now() + '.png';
          if (!shot.png.writeToFileAtomically(path, true)) {
            throw new Error('cannot write ' + path);
          }
          shot.path = path;
          console.log('image ' + path);
        }
        last = write(pb, active ? withPath(shot) : shot.items);
        shown = active;
      }
    } catch (error) {
      console.log('error ' + error);
      shot = null;
      shown = false;
    }
    // While Raido has the front, or the path is on the clipboard, look again soon;
    // otherwise until Raido writes or an app comes to the front (a second at most).
    var wait = active || shown ? 0.25 : 1;
    $.NSRunLoop.currentRunLoop.runModeBeforeDate(
      $.NSDefaultRunLoopMode, $.NSDate.dateWithTimeIntervalSinceNow(wait));
  }
  if (shot && shown && pb.changeCount === last) {
    write(pb, shot.items);
  }
}
