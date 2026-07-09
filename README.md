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

## Distribution — un seul exécutable

Le code user-mode de WinDivert est **lié statiquement** dans l'exe, et le driver
`WinDivert64.sys` est **embarqué** (`include_bytes!`) puis extrait à côté de
l'exe au premier lancement. On distribue donc **un unique `.exe`** (release
GitHub par ex.) : pas de DLL ni de fichiers annexes à joindre.

> Le `.sys` est un driver noyau : Windows le charge depuis un fichier sur disque
> (jamais depuis la mémoire). L'exe le dépose lui-même — invisible pour
> l'utilisateur, et les droits admin déjà requis suffisent à l'écrire.

## Prérequis

- **Utilisateur final** : Windows x64 + droits administrateur au lancement
  (WinDivert charge un driver noyau — popup UAC au premier run). Rien d'autre.
- **Machine de build** : Rust ≥ 1.85 et les MSVC Build Tools (`cl.exe`) — le lien
  statique compile WinDivert depuis ses sources C.

## Build

```sh
cargo build --release
```

Le lien statique est activé par `WINDIVERT_STATIC` dans `.cargo/config.toml`.
Pour compiler/tester le pipeline sans le backend natif (aucun MSVC requis) :

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
