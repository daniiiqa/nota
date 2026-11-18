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
  Ctrl+N otra · Ctrl+L la lista · Ctrl+W cerrar · Ctrl+Shift+S compartir. Una nota nueva que se cierra
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

## Compartidas entre tus compus

Las notas de `Compartidas/` (`Shared/` en inglés) se sincronizan solas entre
tus compus. **Ctrl+Shift+S** en una nota (o `nota share NOTA`) la mueve ahí,
con sus imágenes; otra vez, la regresa.

`nota sync` las cuida de fondo (discada lo arranca al entrar):

- **En vivo, en la misma red**: las dos compus se encuentran por mDNS (como
  picha) y se quedan conectadas. Cada nota compartida es un documento CRDT
  (yrs, el de Yjs): lo que escribes viaja como operación y se junta letra por
  letra con lo que la otra escribe al mismo tiempo, como en Google Docs. De
  teclear aquí a verlo allá, ~¼ de segundo. Todo va cifrado
  (ChaCha20-Poly1305) con la clave del repo (`.nota/clave`): solo tus compus
  se entienden.
- La ventana de nota guarda las compartidas casi al teclear y, si el archivo
  cambió por debajo, junta lo tuyo con lo que llegó sin mover el cursor.
- **Sin verse** (la laptop en la calle): commit cuando dejas de escribir y al
  repo privado (lo trae cada 45 s). Al reencontrarse, primero se emparejan por
  git y luego siguen en vivo.

Si las dos cambiaron lo mismo sin verse no se pierde nada: una versión se queda
y la otra va al lado, «nota (conflicto senjougahara).md». Todo queda en el
historial de git.

Para empezar, una vez:

```bash
gh repo create notas --private          # el repo, privado
nota sync init https://github.com/TU_USUARIO/notas.git   # en cada compu
nota sync status                        # con quién habla y cuándo fue lo último
```

El idioma sale de `LANGUAGE=` en `~/.config/hypr/discada.conf`. Para pegar
imágenes hace falta `wl-clipboard`.
