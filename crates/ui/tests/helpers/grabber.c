/* Stands in for the desktop around the popup (docs/ui.md, "The popup"):
 *   grabber grab MS   holds the keyboard, as a hotkey daemon's grab is still held for a moment
 *                     after the hotkey fires; prints "grabbed", lets go and exits after MS ms
 *   grabber focus MS  takes the input focus, as the application window a window manager keeps
 *                     focused would have it; prints "focused", exits after MS ms */
#include <gtk/gtk.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

static gboolean done(gpointer unused) {
  (void)unused;
  gtk_main_quit();
  return G_SOURCE_REMOVE;
}

int main(int argc, char **argv) {
  gtk_init(&argc, &argv);
  if (argc != 3) {
    return 2;
  }
  int focus = strcmp(argv[1], "focus") == 0;
  GtkWidget *w = gtk_window_new(GTK_WINDOW_POPUP);
  gtk_window_move(GTK_WINDOW(w), 0, 0);
  gtk_window_resize(GTK_WINDOW(w), 1, 1);
  gtk_widget_show(w);
  GdkWindow *gw = gtk_widget_get_window(w);
  GdkSeat *seat = gdk_display_get_default_seat(gdk_window_get_display(gw));
  if (focus) {
    /* Without a window manager, GDK sets the X input focus itself. */
    gdk_window_focus(gw, GDK_CURRENT_TIME);
    gdk_display_sync(gdk_window_get_display(gw));
    printf("focused\n");
  } else {
    if (gdk_seat_grab(seat, gw, GDK_SEAT_CAPABILITY_KEYBOARD, FALSE, NULL, NULL, NULL, NULL) !=
        GDK_GRAB_SUCCESS) {
      return 1;
    }
    printf("grabbed\n");
  }
  fflush(stdout);
  g_timeout_add((guint)atoi(argv[2]), done, NULL);
  gtk_main();
  if (!focus) {
    gdk_seat_ungrab(seat);
  }
  return 0;
}
