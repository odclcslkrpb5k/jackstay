/* A GTK 3 clipboard client for the conformance matrix (docs/conformance.md), so that GTK 3's own
 * clipboard code is what the daemon meets.
 *
 *   clip-gtk3 copy TARGET FILE [TARGET FILE...]  own CLIPBOARD with those targets, print "owned",
 *                                                and quit as an application does when stdin ends
 *                                                (GTK 3 then hands its targets to the manager)
 *   clip-gtk3 paste TARGET                       the target's bytes on stdout; exit 1 if refused
 *   clip-gtk3 paste-text TARGET                  the target as text, decoded by GTK (and Xlib,
 *                                                for COMPOUND_TEXT) by its reply's type, as UTF-8
 *   clip-gtk3 targets                            the owner's targets, one a line
 *
 * A copy's TARGET may instead be `text` (FILE's UTF-8 through gtk_selection_data_set_text), `image`
 * (FILE, a PNG, through set_pixbuf) or `uris` (FILE's lines through set_uris): GTK's own
 * conversions, as applications use them.
 *
 * Exit 3 if no answer comes within 20 s. */
#include <glib-unix.h>
#include <gtk/gtk.h>
#include <stdio.h>
#include <string.h>
#include <unistd.h>

typedef enum { RAW, TEXT, IMAGE, URIS } Kind;

typedef struct {
  const char *target;
  Kind kind;
  gchar *bytes;
  gsize len;
  GdkPixbuf *pixbuf;
  gchar **uris;
} Item;

static Item *items;
static int status = 3;

static gboolean timed_out(gpointer unused) {
  (void)unused;
  fprintf(stderr, "clip-gtk3: no answer\n");
  gtk_main_quit();
  return G_SOURCE_REMOVE;
}

static void get(GtkClipboard *cb, GtkSelectionData *sd, guint info, gpointer unused) {
  (void)cb;
  (void)unused;
  Item *it = &items[info];
  switch (it->kind) {
  case TEXT:
    gtk_selection_data_set_text(sd, it->bytes, (gint)it->len);
    break;
  case IMAGE:
    gtk_selection_data_set_pixbuf(sd, it->pixbuf);
    break;
  case URIS:
    gtk_selection_data_set_uris(sd, it->uris);
    break;
  case RAW:
    gtk_selection_data_set(sd, gdk_atom_intern(it->target, FALSE), 8, (const guchar *)it->bytes,
                           (gint)it->len);
    break;
  }
}

static void cleared(GtkClipboard *cb, gpointer unused) {
  (void)cb;
  (void)unused;
}

static gboolean stdin_ready(gint fd, GIOCondition cond, gpointer unused) {
  (void)cond;
  (void)unused;
  char buf[256];
  if (read(fd, buf, sizeof buf) <= 0) {
    status = 0;
    gtk_main_quit();
    return G_SOURCE_REMOVE;
  }
  return G_SOURCE_CONTINUE;
}

static void received(GtkClipboard *cb, GtkSelectionData *sd, gpointer unused) {
  (void)cb;
  (void)unused;
  gint len = gtk_selection_data_get_length(sd);
  if (len < 0) {
    status = 1;
  } else {
    fwrite(gtk_selection_data_get_data(sd), 1, (size_t)len, stdout);
    status = 0;
  }
  gtk_main_quit();
}

static void received_text(GtkClipboard *cb, GtkSelectionData *sd, gpointer unused) {
  (void)cb;
  (void)unused;
  guchar *text = gtk_selection_data_get_text(sd);
  if (text == NULL) {
    status = 1;
  } else {
    fputs((const char *)text, stdout);
    g_free(text);
    status = 0;
  }
  gtk_main_quit();
}

static void got_targets(GtkClipboard *cb, GdkAtom *atoms, gint n, gpointer unused) {
  (void)cb;
  (void)unused;
  for (gint i = 0; i < n; i++) {
    gchar *name = gdk_atom_name(atoms[i]);
    printf("%s\n", name);
    g_free(name);
  }
  status = atoms == NULL ? 1 : 0;
  gtk_main_quit();
}

int main(int argc, char **argv) {
  gtk_init(&argc, &argv);
  GtkClipboard *cb = gtk_clipboard_get(GDK_SELECTION_CLIPBOARD);
  if (argc >= 4 && strcmp(argv[1], "copy") == 0 && argc % 2 == 0) {
    int n = (argc - 2) / 2;
    items = g_new0(Item, n);
    GtkTargetList *list = gtk_target_list_new(NULL, 0);
    for (int i = 0; i < n; i++) {
      Item *it = &items[i];
      it->target = argv[2 + 2 * i];
      if (!g_file_get_contents(argv[3 + 2 * i], &it->bytes, &it->len, NULL)) {
        fprintf(stderr, "clip-gtk3: cannot read %s\n", argv[3 + 2 * i]);
        return 2;
      }
      if (strcmp(it->target, "text") == 0) {
        it->kind = TEXT;
        gtk_target_list_add_text_targets(list, (guint)i);
      } else if (strcmp(it->target, "image") == 0) {
        it->kind = IMAGE;
        it->pixbuf = gdk_pixbuf_new_from_file(argv[3 + 2 * i], NULL);
        if (it->pixbuf == NULL) {
          fprintf(stderr, "clip-gtk3: not an image: %s\n", argv[3 + 2 * i]);
          return 2;
        }
        gtk_target_list_add_image_targets(list, (guint)i, TRUE);
      } else if (strcmp(it->target, "uris") == 0) {
        it->kind = URIS;
        g_strchomp(it->bytes);
        it->uris = g_strsplit(it->bytes, "\n", -1);
        gtk_target_list_add_uri_targets(list, (guint)i);
      } else {
        it->kind = RAW;
        gtk_target_list_add(list, gdk_atom_intern(it->target, FALSE), 0, (guint)i);
      }
    }
    gint count = 0;
    GtkTargetEntry *entries = gtk_target_table_new_from_list(list, &count);
    if (!gtk_clipboard_set_with_data(cb, entries, (guint)count, get, cleared, NULL)) {
      return 1;
    }
    gtk_clipboard_set_can_store(cb, NULL, 0);
    printf("owned\n");
    fflush(stdout);
    g_unix_fd_add(0, G_IO_IN | G_IO_HUP | G_IO_ERR, stdin_ready, NULL);
    gtk_main();
    return status;
  }
  if (argc == 3 && strcmp(argv[1], "paste") == 0) {
    gtk_clipboard_request_contents(cb, gdk_atom_intern(argv[2], FALSE), received, NULL);
  } else if (argc == 3 && strcmp(argv[1], "paste-text") == 0) {
    gtk_clipboard_request_contents(cb, gdk_atom_intern(argv[2], FALSE), received_text, NULL);
  } else if (argc == 2 && strcmp(argv[1], "targets") == 0) {
    gtk_clipboard_request_targets(cb, got_targets, NULL);
  } else {
    fprintf(stderr, "usage: clip-gtk3 copy TARGET FILE... | paste TARGET | paste-text TARGET | targets\n");
    return 2;
  }
  g_timeout_add_seconds(20, timed_out, NULL);
  gtk_main();
  return status;
}
