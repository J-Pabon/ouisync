#include "snapshot.h"
#include "hash.h"
#include "hex.h"
#include "variant.h"
#include "refcount.h"
#include "archive.h"
#include "random.h"
#include "branch_io.h"
#include "object/tree.h"
#include "object/blob.h"
#include "object/io.h"
#include "ouisync_assert.h"
#include "ostream/set.h"
#include "ostream/map.h"

#include <boost/filesystem/fstream.hpp>
#include <boost/serialization/set.hpp>
#include <boost/serialization/map.hpp>
#include <boost/serialization/array.hpp>
#include <boost/serialization/vector.hpp>

#include <iostream>

using namespace ouisync;
using namespace std;
using object::Tree;
using object::Blob;

static auto _generate_random_name_tag()
{
    Snapshot::NameTag rnd;
    random::generate_non_blocking(rnd.data(), rnd.size());
    return rnd;
}

static auto _path_from_tag(const Snapshot::NameTag& name_tag, const fs::path& dir)
{
    auto hex = to_hex<char>(name_tag);
    return dir / fs::path(hex.begin(), hex.end());
}

ObjectId Snapshot::calculate_id() const
{
    Sha256 hash;
    hash.update("Snapshot");
    hash.update(_commit.root_id);

    hash.update(uint32_t(_nodes.size()));
    for (auto& [id, n] : _nodes) {
        hash.update(static_cast<std::underlying_type_t<NodeType>>(n.type));
        hash.update(id);
    }

    return hash.close();
}

Snapshot::Snapshot(fs::path objdir, fs::path snapshotdir, Commit commit) :
    _name_tag(_generate_random_name_tag()),
    _path(_path_from_tag(_name_tag, snapshotdir)),
    _objdir(std::move(objdir)),
    _snapshotdir(std::move(snapshotdir)),
    _commit(move(commit))
{
    _nodes.insert({_commit.root_id, {NodeType::Missing, {}, {}}});
}

Snapshot::Snapshot(Snapshot&& other) :
    _name_tag(other._name_tag),
    _path(std::move(other._path)),
    _objdir(std::move(other._objdir)),
    _snapshotdir(std::move(other._snapshotdir)),
    _commit(move(other._commit)),
    _nodes(move(other._nodes))
{
}

Snapshot& Snapshot::operator=(Snapshot&& other)
{
    forget();

    _name_tag = other._name_tag;
    _path     = std::move(other._path);
    _objdir   = std::move(other._objdir);
    _commit   = std::move(other._commit);
    _nodes  = move(other._nodes);

    return *this;
}

/* static */
Snapshot Snapshot::create(Commit commit, Options::Snapshot options)
{
    Snapshot s(std::move(options.objectdir),
            move(options.snapshotdir), std::move(commit));

    s.store();
    return s;
}

void Snapshot::notify_parent_that_child_completed(const ObjectId& parent_id, const ObjectId& child)
{
    cerr << "    notify_parent_that_child_completed " << __LINE__ << " parent:" << parent_id << " child:" << child << "\n";
    auto& parent = _nodes.at(parent_id);

    size_t cnt = 0;

    auto i = parent.children.missing.find(child);

    if (i != parent.children.missing.end()) {
        parent.children.missing.erase(i);
        cnt++;
    }

    i = parent.children.incomplete.find(child);

    if (i != parent.children.incomplete.end()) {
        parent.children.incomplete.erase(i);
        cnt++;
    }

    ouisync_assert(cnt == 1);

    parent.children.complete.insert(child);

    if (parent.is_complete()) {
        Rc rc = Rc::load(_objdir, parent_id);

        rc.decrement_direct_count();
        rc.increment_recursive_count();

        ouisync_assert(parent.type == NodeType::Incomplete);

        parent.type = NodeType::Complete;

        auto grandparents = parent.parents;

        for (auto grandparent : grandparents) {
            notify_parent_that_child_completed(grandparent, parent_id);
        }

        _nodes.erase(child);
    }
}

std::set<ObjectId> Snapshot::children_of(const ObjectId& id) const
{
    auto obj = object::io::load<Tree, Blob::Nothing>(_objdir, id);
    if (auto tree = boost::get<Tree>(&obj)) {
        return tree->children();
    }
    return {};
}

void Snapshot::increment_recursive_count(const ObjectId& id) const
{
    Rc::load(_objdir, id).increment_recursive_count();
}

void Snapshot::increment_direct_count(const ObjectId& id) const
{
    Rc::load(_objdir, id).increment_direct_count();
}

Snapshot::Children Snapshot::sort_children(const set<ObjectId>& children) const
{
    Children ret;

    for (auto ch : children) {
        if (!object::io::exists(_objdir, ch)) {
            ret.missing.insert(ch);
            continue;
        }

        Rc rc = Rc::load(_objdir, ch);

        if (rc.recursive_count() > 0) {
            ret.complete.insert(ch);
            continue;
        }

        ret.incomplete.insert(ch);
    }

    return ret;
}

void Snapshot::insert_object(const ObjectId& id, set<ObjectId> children)
{
    cerr <<"----------------------\n";
    std::cerr << "insert_object " << __LINE__ << " id:" << id << " children:" << children << "\n";

    std::cerr << *this << "\n";

    auto i = _nodes.find(id);

    if (i == _nodes.end() || i->second.type != NodeType::Missing) return;

    auto& node = i->second;

    node.type = children.empty() ? NodeType::Complete : NodeType::Incomplete;
    node.children = sort_children(children);

    for (auto& child_id : children) {
        auto [child_i, inserted] = _nodes.insert({child_id, {NodeType::Missing, {}, {}}});
        child_i->second.parents.insert(id);
    }

    if (node.is_complete()) {
        increment_recursive_count(id);
        // If a parent becomes complete as well, they'll try to remove
        // `node` from `_nodes`. Thus we need to create copy of parents.
        auto parents = node.parents;
        for (auto& parent_id : parents) {
            notify_parent_that_child_completed(parent_id, id);
        }
    } else {
        increment_direct_count(id);
    }

    std::cerr << *this << "\n";
}

void Snapshot::store()
{
    archive::store(_path, _nodes);
}

void Snapshot::forget() noexcept
{
    auto nodes = move(_nodes);

    try {
        for (auto& [id, node] : nodes) {
            switch (node.type) {
                case NodeType::Complete: {
                    refcount::deep_remove(_objdir, id);
                }
                break;

                case NodeType::Incomplete: {
                    refcount::flat_remove(_objdir, id);
                }
                break;

                case NodeType::Missing: {
                }
                break;
            }
        }
    }
    catch (const std::exception& e) {
        exit(1);
    }
}

Snapshot Snapshot::clone() const
{
    Snapshot c(_objdir, _snapshotdir, _commit);

    for (auto& [id, node] : _nodes) {
        c._nodes.insert({id, node});

        switch (node.type) {
            case NodeType::Complete: {
                increment_recursive_count(id);
            }
            break;

            case NodeType::Incomplete: {
                increment_direct_count(id);
            }
            break;

            case NodeType::Missing: {
            }
            break;
        }
    }

    return c;
}

void Snapshot::sanity_check() const
{
    //for (auto& [id, _] : _objects.incomplete) {
    //    ouisync_assert(object::io::exists(_objdir, id));
    //}

    //for (auto& id : _objects.complete) {
    //    ouisync_assert(object::io::is_complete(_objdir, id));
    //}
}

Snapshot::~Snapshot()
{
    forget();
}

std::ostream& ouisync::operator<<(std::ostream& os, const Snapshot& s)
{
    os << "Snapshot: " << s._commit << "\n";
    os << BranchIo::Immutable(s._objdir, s._commit.root_id);
    for (auto& [id, n] : s._nodes) {
        os << id << ": " << n << "\n";
    }
    return os;
}

std::ostream& ouisync::operator<<(std::ostream& os, Snapshot::NodeType t)
{
    switch (t) {
        case Snapshot::NodeType::Missing:    return os << "Missing";
        case Snapshot::NodeType::Incomplete: return os << "Incomplete";
        case Snapshot::NodeType::Complete:   return os << "Complete";
    }
    return os;
}

std::ostream& ouisync::operator<<(std::ostream& os, const Snapshot::Node& n)
{
    os << "Node{" << n.type << ", ";
    os << "parents: " << n.parents << ", ";
    os << "children: " << n.children << "}";
    return os;
}

std::ostream& ouisync::operator<<(std::ostream& os, const Snapshot::Children& ch)
{
    os << "Children{";
    os << "missing: "    << ch.missing    << ", ";
    os << "incomplete: " << ch.incomplete << ", ";
    os << "complete: "   << ch.complete   << "}";
    return os;
}

////////////////////////////////////////////////////////////////////////////////
// SnapshotGroup

SnapshotGroup::Id SnapshotGroup::calculate_id() const
{
    Sha256 hash;
    hash.update("SnapshotGroup");
    hash.update(uint32_t(size()));
    for (auto& [user_id, snapshot] : *this) {
        hash.update(user_id.to_string());
        hash.update(snapshot.calculate_id());
    }
    return hash.close();
}

SnapshotGroup::~SnapshotGroup()
{
    for (auto& [_, s] : static_cast<Parent&>(*this)) {
        s.forget();
    }
}

std::ostream& ouisync::operator<<(std::ostream& os, const SnapshotGroup& g)
{
    os << "SnapshotGroup{id:" << g.id() << " [";
    bool is_first = true;
    for (auto& [user_id, snapshot] : g) {
        if (!is_first) { os << ", "; }
        is_first = false;
        os << "(" << user_id << ", " << snapshot << ")";
    }
    return os << "]}";
}
