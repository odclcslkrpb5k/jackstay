"""Drives a running LibreOffice through UNO for the matrix's application tier
(docs/conformance.md, crates/jackstayd/tests/applications.rs).

    office.py PIPE copy-text FILE    a new Writer document holding FILE's text, all of it copied
    office.py PIPE copy-image PNG    a new Writer document holding the image, the image copied
    office.py PIPE paste             a new Writer document, pasted into; prints what it holds:
                                     "graphics N WxH" (the first graphic's pixels, or "-") on a
                                     line, then the document's text
    office.py PIPE quit              LibreOffice's own exit

Copying and pasting are LibreOffice's own .uno:Copy and .uno:Paste, as its Edit menu does.
"""

import sys
import time

import uno
from com.sun.star.beans import PropertyValue


def prop(name, value):
    p = PropertyValue()
    p.Name = name
    p.Value = value
    return p


def connect(pipe):
    local = uno.getComponentContext()
    resolver = local.ServiceManager.createInstanceWithContext(
        "com.sun.star.bridge.UnoUrlResolver", local
    )
    deadline = time.monotonic() + 30
    while True:
        try:
            return resolver.resolve(
                "uno:pipe,name=%s;urp;StarOffice.ComponentContext" % pipe
            )
        except Exception:
            if time.monotonic() > deadline:
                raise
            time.sleep(0.1)


def main():
    pipe, command = sys.argv[1], sys.argv[2]
    ctx = connect(pipe)
    smgr = ctx.ServiceManager
    desktop = smgr.createInstanceWithContext("com.sun.star.frame.Desktop", ctx)
    if command == "quit":
        # As a person answering Don't Save: a modified document would stop the exit to ask.
        documents = desktop.Components.createEnumeration()
        while documents.hasMoreElements():
            documents.nextElement().setModified(False)
        desktop.terminate()
        return
    dispatch = smgr.createInstanceWithContext("com.sun.star.frame.DispatchHelper", ctx)
    doc = desktop.loadComponentFromURL("private:factory/swriter", "_blank", 0, ())
    frame = doc.CurrentController.Frame
    if command == "copy-text":
        with open(sys.argv[3], encoding="utf-8", newline="") as f:
            doc.Text.setString(f.read())
        dispatch.executeDispatch(frame, ".uno:SelectAll", "", 0, ())
        dispatch.executeDispatch(frame, ".uno:Copy", "", 0, ())
    elif command == "copy-image":
        provider = smgr.createInstanceWithContext("com.sun.star.graphic.GraphicProvider", ctx)
        graphic = provider.queryGraphic((prop("URL", uno.systemPathToFileUrl(sys.argv[3])),))
        shape = doc.createInstance("com.sun.star.text.TextGraphicObject")
        shape.Graphic = graphic
        doc.Text.insertTextContent(doc.Text.End, shape, False)
        doc.CurrentController.select(shape)
        dispatch.executeDispatch(frame, ".uno:Copy", "", 0, ())
    elif command == "paste":
        dispatch.executeDispatch(frame, ".uno:Paste", "", 0, ())
        graphics = doc.GraphicObjects
        size = "-"
        if graphics.Count:
            pixels = graphics.getByIndex(0).Graphic.SizePixel
            size = "%dx%d" % (pixels.Width, pixels.Height)
        out = "graphics %d %s\n%s" % (graphics.Count, size, doc.Text.getString())
        sys.stdout.buffer.write(out.encode("utf-8"))
    else:
        sys.exit("unknown command " + command)


main()
