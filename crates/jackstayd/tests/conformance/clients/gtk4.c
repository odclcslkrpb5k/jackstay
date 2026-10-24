/* A GTK 4 clipboard client for the conformance matrix (docs/conformance.md). The same commands as
 * clip-gtk3, with MIME types for targets, as GTK 4 names them:
 *
 *   clip-gtk4 copy MIME FILE [MIME FILE...] | paste MIME | targets
 *
 * A copy's MIME may instead be `text` (FILE's UTF-8 as a string), `image` (FILE, a PNG, as a
 * texture) or `uris` (FILE's lines as a file list): GTK's own serializers, as applications use them.
 *
 * Exit 3 if no answer comes within 20 s. */
#include <glib-unix.h>
#include <gio/gunixoutputstream.h>
#include <gtk/gtk.h>
#include <stdio.h>
#include <string.h>
#include <unistd.h>

static GMainLoop *loop;
static int status = 3;

static gboolean timed_out(gpointer unused) {
  (void)unused;
  fprintf(stderr, "clip-gtk4: no answer\n");
  g_main_loop_quit(loop);
  return G_SOURCE_REMOVE;
}

static gboolean stdin_ready(gint fd, GIOCondition cond, gpointer unused) {
  (void)cond;
  (void)unused;
  char buf[256];
  if (read(fd, buf, sizeof buf) <= 0) {
    status = 0;
    g_main_loop_quit(loop);
    return G_SOURCE_REMOVE;
  }
  return G_SOURCE_CONTINUE;
}

static void spliced(GObject *out, GAsyncResult *res, gpointer unused) {
  (void)unused;
  status = g_output_stream_splice_finish(G_OUTPUT_STREAM(out), res, NULL) < 0 ? 1 : 0;
  g_main_loop_quit(loop);
}

static void read_done(GObject *cb, GAsyncResult *res, gpointer unused) {
  (void)unused;
  GError *error = NULL;
  GInputStream *in = gdk_clipboard_read_finish(GDK_CLIPBOARD(cb), res, NULL, &error);
  if (in == NULL) {
    fprintf(stderr, "clip-gtk4: %s\n", error->message);
    status = 1;
    g_main_loop_quit(loop);
    return;
  }
  GOutputStream *out = g_unix_output_stream_new(1, FALSE);
  g_output_stream_splice_async(out, in, G_OUTPUT_STREAM_SPLICE_CLOSE_SOURCE, G_PRIORITY_DEFAULT,
                               NULL, spliced, NULL);
}

static gboolean print_targets(gpointer cb) {
  GdkContentFormats *formats = gdk_clipboard_get_formats(GDK_CLIPBOARD(cb));
  gsize n = 0;
  const char *const *mimes = gdk_content_formats_get_mime_types(formats, &n);
  for (gsize i = 0; i < n; i++) {
    printf("%s\n", mimes[i]);
  }
  status = n > 0 ? 0 : 1;
  g_main_loop_quit(loop);
  return G_SOURCE_REMOVE;
}

static void formats_changed(GObject *cb, GParamSpec *spec, gpointer unused) {
  (void)spec;
  (void)unused;
  print_targets(cb);
}

int main(int argc, char **argv) {
  gtk_init();
  loop = g_main_loop_new(NULL, FALSE);
  GdkClipboard *cb = gdk_display_get_clipboard(gdk_display_get_default());
  if (argc >= 4 && strcmp(argv[1], "copy") == 0 && argc % 2 == 0) {
    int n = (argc - 2) / 2;
    GdkContentProvider **providers = g_new0(GdkContentProvider *, n);
    for (int i = 0; i < n; i++) {
      gchar *bytes;
      gsize len;
      if (!g_file_get_contents(argv[3 + 2 * i], &bytes, &len, NULL)) {
        fprintf(stderr, "clip-gtk4: cannot read %s\n", argv[3 + 2 * i]);
        return 2;
      }
      const char *mime = argv[2 + 2 * i];
      if (strcmp(mime, "text") == 0) {
        providers[i] = gdk_content_provider_new_typed(G_TYPE_STRING, bytes);
      } else if (strcmp(mime, "image") == 0) {
        GdkTexture *texture = gdk_texture_new_from_filename(argv[3 + 2 * i], NULL);
        if (texture == NULL) {
          fprintf(stderr, "clip-gtk4: not an image: %s\n", argv[3 + 2 * i]);
          return 2;
        }
        providers[i] = gdk_content_provider_new_typed(GDK_TYPE_TEXTURE, texture);
      } else if (strcmp(mime, "uris") == 0) {
        g_strchomp(bytes);
        gchar **uris = g_strsplit(bytes, "\n", -1);
        GSList *files = NULL;
        for (gchar **u = uris; *u != NULL; u++) {
          files = g_slist_append(files, g_file_new_for_uri(*u));
        }
        providers[i] = gdk_content_provider_new_typed(GDK_TYPE_FILE_LIST, files);
      } else {
        providers[i] = gdk_content_provider_new_for_bytes(mime, g_bytes_new_take(bytes, len));
      }
    }
    gdk_clipboard_set_content(cb, gdk_content_provider_new_union(providers, (gsize)n));
    printf("owned\n");
    fflush(stdout);
    g_unix_fd_add(0, G_IO_IN | G_IO_HUP | G_IO_ERR, stdin_ready, NULL);
    g_main_loop_run(loop);
    return status;
  }
  if (argc == 3 && strcmp(argv[1], "paste") == 0) {
    const char *mimes[] = {argv[2], NULL};
    gdk_clipboard_read_async(cb, mimes, G_PRIORITY_DEFAULT, NULL, read_done, NULL);
  } else if (argc == 2 && strcmp(argv[1], "targets") == 0) {
    /* A foreign owner's formats arrive after start; whatever is known in 2 s is printed. */
    g_signal_connect(cb, "notify::formats", G_CALLBACK(formats_changed), NULL);
    g_timeout_add_seconds(2, print_targets, cb);
  } else {
    fprintf(stderr, "usage: clip-gtk4 copy MIME FILE... | paste MIME | targets\n");
    return 2;
  }
  g_timeout_add_seconds(20, timed_out, NULL);
  g_main_loop_run(loop);
  return status;
}
