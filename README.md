# nota

Las notas de [discada](https://github.com/daniiiqa/dotfiles), como notas
pegadas en el escritorio: cada nota es su propia ventana que flota donde la
pongas, se puede fijar (que se vea en todos los workspaces) y lleva imágenes.
Dibujada a mano con el look del rice (colores, fuente y estilo de cada look,
vía [disco](https://github.com/daniiiqa/disco)), como jale o arre.

```
nota                 la lista de notas; cada una abre en su ventana
nota new             una nota nueva
nota open NOTA       abrir esa nota
nota restore         volver a poner las que estaban abiertas, donde estaban
nota tui             las notas en la terminal (abren en $EDITOR)
nota capture         una cajita para apuntar algo al vuelo, encima de todo
nota TEXTO…          apuntar TEXTO en la bandeja (con fecha y hora)
nota -               apuntar lo que llegue por stdin
nota list            las notas: título<TAB>ruta, la última primero
nota dir             la carpeta de las notas
```

En discada: **SUPER+N** nota nueva, **SUPER+ALT+N** la lista. Al entrar,
`nota restore` vuelve a poner las notas que dejaste abiertas.

## Una nota

- El título se arrastra para moverla; las orillas, para el tamaño. Botones:
  fijar, otra nota, la lista, cerrar.
- Se guarda sola mientras escribes. Una nota nueva toma el nombre de su primer
  renglón; doble clic en el título (o F2) la renombra.
- **Imágenes**: Ctrl+V pega una captura o una imagen copiada (o archivos de
  imagen copiados en taco). Se guardan en `attachments/` y en la nota queda un
  renglón `![](attachments/…png)`. También se ven las de Obsidian (`![[…]]`).
  Clic en una imagen la elige; Retroceso o Supr la quitan.
- Ctrl+Z / Ctrl+Shift+Z deshacer y rehacer · Ctrl+C/X copiar, cortar ·
  Ctrl+A todo · Ctrl+Inicio/Fin el principio o el final · Ctrl+P fijar ·
  Ctrl+N otra · Ctrl+L la lista · Ctrl+W cerrar. Una nota nueva que se cierra
  vacía no se queda.

La lista: escribir busca (en el título y en el texto), Enter o clic abre,
Enter sin nada que case la crea con ese nombre, Ctrl+D dos veces la manda a la
basura.

Un solo proceso para todas las ventanas: el primero escucha en
`$XDG_RUNTIME_DIR/nota.sock` y los demás le mandan qué abrir. Dónde quedó cada
nota (y si estaba fija) va en `~/.local/state/nota/open.json`.

## Las notas

Archivos `.md` en `~/Notas` (`~/Notes` en inglés; `NOTA_DIR` la cambia). No hay
base de datos ni nada propio: el título es el nombre del archivo, como en
Obsidian, así que la misma carpeta:

- abre tal cual como vault de Obsidian (imágenes incluidas),
- se sincroniza con Git, Syncthing o Relay,
- la puede leer y editar un agente (Claude Code, compa…).

Lo que cambie por fuera aparece solo en la ventana abierta. Lo borrado va a
`.trash/` (la misma basura que Obsidian).

El idioma sale de `LANGUAGE=` en `~/.config/hypr/discada.conf`. Para pegar
imágenes hace falta `wl-clipboard`.
