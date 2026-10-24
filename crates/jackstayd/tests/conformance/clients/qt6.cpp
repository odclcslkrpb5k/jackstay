// A Qt 6 clipboard client for the conformance matrix (docs/conformance.md). The same commands as
// clip-gtk4, with MIME types as Qt names them:
//
//   clip-qt6 copy MIME FILE [MIME FILE...] | paste MIME | targets
//
// A copy's MIME may instead be `text` (setText), `image` (FILE, a PNG, through setImageData) or
// `uris` (FILE's lines through setUrls): Qt's own conversions, as applications use them.
//
// Quitting after a copy goes through QGuiApplication's own exit, which hands the clipboard to
// the manager (SAVE_TARGETS), as a Qt application's does.
#include <QClipboard>
#include <QFile>
#include <QGuiApplication>
#include <QImage>
#include <QUrl>
#include <QMimeData>
#include <QSocketNotifier>
#include <QTimer>
#include <cstdio>
#include <unistd.h>

int main(int argc, char **argv) {
  QGuiApplication app(argc, argv);
  QStringList args = app.arguments();
  QClipboard *cb = QGuiApplication::clipboard();
  if (args.size() >= 4 && args[1] == "copy" && args.size() % 2 == 0) {
    auto *data = new QMimeData;
    for (int i = 2; i < args.size(); i += 2) {
      QFile f(args[i + 1]);
      if (!f.open(QIODevice::ReadOnly)) {
        std::fprintf(stderr, "clip-qt6: cannot read %s\n", qPrintable(args[i + 1]));
        return 2;
      }
      QByteArray bytes = f.readAll();
      if (args[i] == "text") {
        data->setText(QString::fromUtf8(bytes));
      } else if (args[i] == "image") {
        QImage image = QImage::fromData(bytes);
        if (image.isNull()) {
          std::fprintf(stderr, "clip-qt6: not an image: %s\n", qPrintable(args[i + 1]));
          return 2;
        }
        data->setImageData(image);
      } else if (args[i] == "uris") {
        QList<QUrl> urls;
        for (const QByteArray &line : bytes.trimmed().split('\n')) {
          urls.append(QUrl::fromEncoded(line));
        }
        data->setUrls(urls);
      } else {
        data->setData(args[i], bytes);
      }
    }
    cb->setMimeData(data, QClipboard::Clipboard);
    std::printf("owned\n");
    std::fflush(stdout);
    QSocketNotifier input(0, QSocketNotifier::Read);
    QObject::connect(&input, &QSocketNotifier::activated, [&] {
      char buf[256];
      if (read(0, buf, sizeof buf) <= 0) {
        input.setEnabled(false);
        app.quit();
      }
    });
    return app.exec();
  }
  if (args.size() == 3 && args[1] == "paste") {
    const QMimeData *data = cb->mimeData(QClipboard::Clipboard);
    if (data == nullptr || !data->hasFormat(args[2])) {
      return 1;
    }
    QByteArray bytes = data->data(args[2]);
    std::fwrite(bytes.constData(), 1, static_cast<size_t>(bytes.size()), stdout);
    return 0;
  }
  if (args.size() == 2 && args[1] == "targets") {
    const QMimeData *data = cb->mimeData(QClipboard::Clipboard);
    if (data == nullptr) {
      return 1;
    }
    for (const QString &f : data->formats()) {
      std::printf("%s\n", qPrintable(f));
    }
    return data->formats().isEmpty() ? 1 : 0;
  }
  std::fprintf(stderr, "usage: clip-qt6 copy MIME FILE... | paste MIME | targets\n");
  return 2;
}
