// Raido's clipboard watcher (macOS, JavaScript for Automation). While an AI pane is
// open, a screenshot (an image with no text) put on the clipboard is saved as a
// PNG in `argv[0]`, and its path is added to the clipboard as text beside the image:
// Terminal.app's ⌘V pastes only text, and the agents attach a pasted image path.
// Apps that take images still get the image. Prints "image <path>" for each one.
// `argv[1]` names a private pasteboard instead of the system clipboard (tests).
// It stops when Raido does: its parent process changes when Raido is gone.
ObjC.import('AppKit');
ObjC.import('unistd');

function run(argv) {
  var keep = argv[0];
  var pb = argv[1]
    ? $.NSPasteboard.pasteboardWithName(argv[1])
    : $.NSPasteboard.generalPasteboard;
  var text = ['public.utf8-plain-text', 'public.plain-text', 'NSStringPboardType', 'public.file-url'];
  var images = ['public.png', 'public.tiff', 'public.jpeg', 'public.heic', 'com.compuserve.gif'];
  var parent = $.getppid();
  var last = -1;
  while ($.getppid() === parent) {
    var count = pb.changeCount;
    if (count !== last) {
      last = count;
      try {
        var types = ObjC.deepUnwrap(pb.types) || [];
        var hasText = types.some(function (t) { return text.indexOf(t) >= 0; });
        var image = images.filter(function (t) { return types.indexOf(t) >= 0; })[0];
        if (image && !hasText) {
          var data = pb.dataForType(image);
          if (image !== 'public.png') {
            var rep = $.NSBitmapImageRep.imageRepWithData(data);
            data = rep.representationUsingTypeProperties($.NSBitmapImageFileTypePNG, $({}));
          }
          var path = keep + '/clipboard-' + Date.now() + '.png';
          if (data.writeToFileAtomically(path, true)) {
            pb.clearContents;
            pb.setDataForType(data, 'public.png');
            pb.setStringForType(path, 'public.utf8-plain-text');
            last = pb.changeCount;
            console.log('image ' + path);
          }
        }
      } catch (error) {
        console.log('error ' + error);
      }
    }
    delay(0.25);
  }
}
