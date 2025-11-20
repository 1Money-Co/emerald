# Emerald - Documentation

To install to view locally:

```bash
python3 -m venv ~/mkdocs-env
source ~/mkdocs-env/bin/activate
pip install mkdocs mkdocs-material mkdocs-awesome-pages-plugin mkdocs-github-admonitions-plugin
mkdocs --version # To verify the installation
```

To view the documentation locally:

```bash
mkdocs serve
```

When you are done, deactivate the virtual environment:

```bash
deactivate
```

### How to Add Documentation

`mkdocs awesome-pages` plugin is used to auto generate the navigation for nested directories in `docs/`.

To add a new page, create a new markdown file in the `docs/{topic}/` directory. The file should have a `.md` extension.

Each directory 1 level into `docs/` should have an `index.md` file. This file should contain the title `Overview` and a brief description of the contents of the directory.

All nested directories only need to contain the documentation files for the topics they cover.
Example `docs/infra/ovh/provision-server.md` will be automatically added to the navigation.

### How to Style Documentation

`mkdocs-github-admonitions-plugin` is used to support GitHub-style admonitions in markdown files. These are rendered consistently in both GitHub and in our MkDocs documentation.

For detailed information on formatting, including:
- Using admonitions (notes, warnings, tips, etc.)
- Code block formatting
- Links and references
- Other styling guidelines

Please refer to the [MkDocs Styling Guide](docs/mkdocs-styling-guide.md) for comprehensive styling guidelines and examples.

## Using Docker Compose

Setup your environment to have the same user inside the container.

```bash
echo "UID=$(id -u)" > .env
echo "GID=$(id -g)" >> .env
echo "USERNAME=$(id -un)" >> .env
```

Start the container, which also builds the image.

```bash
docker compose up -d
```

