# Arkyve — Refresh Shop

Relais local du Secret Shop d'Epic Seven. **Strictement passif et en lecture
seule** : il observe une copie du trafic du jeu, transmet le flux brut à un
serveur d'analyse, et affiche les alertes renvoyées. Il n'automatise rien,
n'envoie aucune donnée vers le jeu, et n'altère jamais ses communications.

## Fonctionnement

```
WinDivert SNIFF ─▶ parse IP/TCP ─▶ réassemblage TCP ─▶ gate ─▶ WebSocket ─▶ serveur
   (bloquant)                        (ordonné/dédup)                   ▲          │
                                                                  alertes ◀───────┘
```

- **Capture** : WinDivert en mode `SNIFF` + `RECV_ONLY` livre une *copie* des
  paquets TCP du port de jeu ; les originaux poursuivent leur route intacts.
- **Réassemblage** : les segments capturés (potentiellement désordonnés ou
  retransmis) sont recomposés en un flux d'octets ordonné, par connexion.
- **Transmission** : le flux brut serveur → client est envoyé tel quel au
  serveur d'analyse. Le déchiffrement et l'interprétation se font **côté
  serveur** — le client ne déchiffre rien.
- **Affichage** : les messages du serveur (instantané du shop, alertes) sont
  rendus en console.

L'interrupteur **Shop Watch** (activé par défaut) coupe la transmission quand le
joueur n'est pas dans le shop.

## Prérequis

- Windows x64, Rust ≥ 1.85.
- **Droits administrateur** au lancement : WinDivert charge un driver noyau
  (popup UAC au premier run).
- Le runtime WinDivert (`WinDivert.dll` + `WinDivert64.sys`) est fourni dans
  `vendor/windivert/` et copié automatiquement à côté de l'exécutable au build.

## Build

```sh
cargo build --release
```

`WINDIVERT_PATH` (défini dans `.cargo/config.toml`) pointe le SDK pour le
linking. Pour compiler/tester le pipeline sans le backend natif :

```sh
cargo test --no-default-features
```

## Configuration

Copier `config.example.toml` en `config.toml` et ajuster. Toutes les clés ont
un défaut ; un fichier absent revient aux valeurs par défaut.

| Clé | Défaut | Rôle |
|-----|--------|------|
| `game_port` | `3333` | Port TCP du serveur de jeu |
| `server_url` | `ws://127.0.0.1:3001/refresh-shop` | Serveur d'analyse |
| `forward.server_to_client` | `true` | Transmettre les réponses (contenu du shop) |
| `forward.client_to_server` | `false` | Transmettre les requêtes (contexte) |

## Lancement

```sh
cargo run --release   # en administrateur
```

Commandes en cours d'exécution : `[Entrée]` bascule Shop Watch · `on` · `off` ·
`Ctrl+C` pour quitter.
